use ethers::{
    abi::Abi,
    contract::{Contract, abigen},
    providers::{Middleware, Provider, Ws},
    signers::{LocalWallet, Signer},
    types::{Transaction, H160, U256},
};
use std::sync::Arc;
use tokio::io::AsyncReadExt;
use serde::{Deserialize, Serialize};
use chrono::Utc;
use anyhow::Result;
use futures_util::stream::StreamExt;

use crate::constants::*;

abigen!(IERC20, "./IERC20.json");


#[derive(Serialize, Deserialize)]
struct ContractABI {
    abi: Vec<serde_json::Value>,
}

pub async fn load_contract_abi() -> Result<Abi> {
    let mut abi_file = tokio::fs::File::open("SniperBotABI.json").await?;
    let mut abi_content = String::new();
    abi_file.read_to_string(&mut abi_content).await?;
    let contract_abi: ContractABI = serde_json::from_str(&abi_content)?;
    Ok(serde_json::from_value(serde_json::Value::Array(contract_abi.abi))?)
}

pub async fn decode_transaction(tx: &Transaction) -> Result<(H160, H160, U256)> {
    let router_abi: Abi = serde_json::from_str(include_str!("../UniswapV2RouterABI.json"))?;

    match router_abi.function("swapExactTokensForTokens") {
        Ok(func) => match func.decode_input(&tx.input) {
            Ok(decoded) => Ok((
                decoded[0].clone().into_address().unwrap_or_default(), 
                decoded[1].clone().into_address().unwrap_or_default(),
                decoded[2].clone().into_uint().unwrap_or_default()
            )),
            Err(e) => {
                eprintln!("Failed to decode transaction: {:?}", e);
                Err(anyhow::anyhow!("Transaction decoding failed"))
            }
        },
        Err(e) => {
            eprintln!("Failed to load UniswapV2Router function: {:?}", e);
            Err(anyhow::anyhow!("ABI function missing"))
        }
    }
}

pub async fn simulate_transaction(
    provider: Arc<Provider<Ws>>,
    pool_address: H160,
    token_in: H160,
    token_out: H160,
    amount_in: U256,
) -> Result<(U256, U256)> {
    let pool_abi: Abi = serde_json::from_str(include_str!("../UniswapV2PairABI.json"))?;
    let pool = Contract::new(pool_address, pool_abi, provider.clone());
    let reserves: (U256, U256, U256) = pool.method("getReserves", ())?.call().await?;

    let (reserve_in, reserve_out) = if token_in < token_out { (reserves.0, reserves.1) } else { (reserves.1, reserves.0) };

    let amount_in_with_fee = amount_in * U256::from(997) / U256::from(1000);
    let amount_out = (amount_in_with_fee * reserve_out) / (reserve_in + amount_in_with_fee);

    Ok((reserve_in + amount_in, reserve_out - amount_out))
}

pub async fn check_price_discrepancy(
    provider: Arc<Provider<Ws>>,
    dex1_pool: H160,
    dex2_pool: H160,
    token_in: H160,
    token_out: H160,
    amount_in: U256,
) -> Result<bool> {
    let (dex1_reserve_in, dex1_reserve_out) = simulate_transaction(
        provider.clone(), 
        dex1_pool, 
        token_in, token_out, 
        amount_in
    ).await?;
    let (dex2_reserve_in, dex2_reserve_out) = simulate_transaction(
        provider.clone(),
         dex2_pool,
        token_in,
         token_out,
         amount_in
        )
        .await?;

    let dex1_price = (dex1_reserve_out * U256::exp10(18)) / dex1_reserve_in;
    let dex2_price = (dex2_reserve_out * U256::exp10(18)) / dex2_reserve_in;

    Ok((dex1_price.max(dex2_price) - dex1_price.min(dex2_price)) > U256::exp10(15))
}

pub async fn execute_arbitrage(
    provider: Arc<Provider<Ws>>,
    contract: Arc<Contract<Provider<Ws>>>,
    wallet: Arc<LocalWallet>,
    token_in: H160,
    token_out: H160,
    amount_in: U256,
) -> Result<()> {
    let gas_price = provider.get_gas_price().await?;
    let deadline = U256::from(Utc::now().timestamp() + 300);
    let mut tx = contract.method::<_, ()>("executeTrade", (token_in, token_out, amount_in, 3000, U256::one(), deadline))?
        .gas_price(gas_price)
        .from(wallet.address())
        .tx;
    let gas_estimate = provider.estimate_gas(&tx, None).await?;
    let tx = tx.set_gas(gas_estimate);

    let signed_tx = wallet.sign_transaction(&tx).await?;
    let raw_tx = tx.rlp_signed(&signed_tx);

    provider.send_raw_transaction(raw_tx).await?;
    Ok(())
}

pub async fn is_target_pair(tx: &Transaction, target_token_in: H160, target_token_out: H160) -> bool {
    if let Ok((token_in, token_out, _)) = decode_transaction(tx).await {
        return (token_in == target_token_in && token_out == target_token_out)
            || (token_in == target_token_out && token_out == target_token_in);
    }
    false
}

async fn check_contract_balance(
    provider: Arc<Provider<Ws>>,
    contract_address: H160,
    token_address: H160,
) -> Result<U256> {
    let token = IERC20::new(token_address, provider.clone());
    let balance = token.balance_of(contract_address).call().await?;
    Ok(balance)
}
pub async fn monitor_mempool(
    provider: Arc<Provider<Ws>>, 
    contract: Arc<Contract<Provider<Ws>>>, 
    wallet: Arc<LocalWallet>, 
    target_token_in: H160, 
    target_token_out: H160,
) {
    match  provider.subscribe_pending_txs().await {
        Ok(mut stream) => {

    while let Some(tx_hash) = stream.next().await {
        let provider = provider.clone();
        let contract = contract.clone();
        let wallet = wallet.clone();
        let target_token_in = target_token_in;
        let target_token_out = target_token_out;

        tokio::spawn(async move {
            if let Ok(Some(tx)) = provider.get_transaction(tx_hash).await {
                if is_target_pair(&tx, target_token_in, target_token_out).await {
                    if let Ok((token_in, token_out, amount_in)) = decode_transaction(&tx).await {
                        match check_price_discrepancy(
                            provider.clone(),
                            UNISWAP_ROUTER.parse().unwrap(),
                            SUSHISWAP_ROUTER.parse().unwrap(),
                            token_in,
                            token_out,
                            amount_in,
                        ).await {
                            Ok(true) => {
                                if let Ok(contract_balance) = check_contract_balance(
                                    provider.clone(),
                                    WETH_ADDRESS.parse::<H160>().unwrap(),
                                    WETH_ADDRESS.parse::<H160>().unwrap(),
                                ).await {
                                    if contract_balance >= amount_in {
                                        if let Err(e) = execute_arbitrage(
                                            provider.clone(), 
                                            contract.clone(), 
                                            wallet.clone(), 
                                            token_in, 
                                            token_out, 
                                            amount_in
                                        ).await {
                                            eprintln!("Failed to execute arbitrage: {:?}", e);
                                        } else {
                                            println!("Arbitrage executed: {} -> {} (amount: {})", token_in, token_out, amount_in);
                                        }
                                    } else {
                                        println!("Insufficient WETH balance for arbitrage: {} -> {} (amount: {})", token_in, token_out, amount_in);
                                    }
                                }
                            }
                            Ok(false) => {
                                println!("No arbitrage opportunity found: {} -> {} (amount: {})", token_in, token_out, amount_in);
                            }
                            Err(e) => {
                                eprintln!("Error checking price discrepancy: {:?}", e);
                            }
                        }
                    }
                }else{
                    eprintln!("Failed to decode transaction: {:?}", tx_hash);

                }
            }else{
                eprintln!("Failed to get transaction: {:?}", tx_hash);
            }
        });
    }
        }
        Err(e) => {
            eprintln!("Failed to subscribe to pending transactions: {:?}", e);
        }
    }


    
}

    
    


