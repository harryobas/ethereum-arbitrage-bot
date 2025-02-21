use ethers::{
    abi::Abi,
    contract:: Contract,
    providers::{Middleware, Provider, Ws},
    signers::{LocalWallet, Signer},
    types::{Transaction, H160, U256},
};
use std::sync::Arc;
use chrono::Utc;
use anyhow::{Result, anyhow};
use futures_util::stream::StreamExt;
use log::{info, warn, error};

use crate::constants::{SUSHISWAP_FACTORY_ADDRESS, UNISWAP_ROUTER, UNISWAP_V2_FACTORY_ADDRESS};
use crate::utils::*;

pub enum TradeDirections {
    UNISWAP,
    SUSHISWAP,
}

pub enum TransactionType {
    UniswapTrade,
    SushiswapTrade,
}

pub fn load_contract_abi() -> Result<Abi> {
    Ok(CONTRACT_ABI.clone())
}

pub fn load_router_abi() -> Result<Abi> {
    Ok(UNISWAP_V2_ROUTER_ABI.clone())
}

pub async fn simulate_transaction(
    provider: Arc<Provider<Ws>>,
    use_sushiswap: bool,
    token_in: H160,
    token_out: H160,
) -> Result<(U256, U256)> {
    let factory_address = if use_sushiswap {
        SUSHISWAP_FACTORY_ADDRESS.parse::<H160>()?
    } else {
        UNISWAP_V2_FACTORY_ADDRESS.parse::<H160>()?
    };

    let pool_address = get_pool_address(provider.clone(), factory_address, token_in, token_out).await?;
    let pool = Contract::new(pool_address, POOL_ABI.clone(), provider.clone());
    let reserves: (U256, U256, U256) = pool.method("getReserves", ())?.call().await?;

    let (reserve_in, reserve_out) = if token_in < token_out {
        (reserves.0, reserves.1)
    } else {
        (reserves.1, reserves.0)
    };

    if reserve_in.is_zero() || reserve_out.is_zero() {
        return Err(anyhow!("Insufficient liquidity in pool"));
    }

    Ok((reserve_in, reserve_out))
}

fn simulate_trade_profit(
    reserve_in: U256,
    reserve_out: U256,
    amount_in: U256,
    fee_rate: U256,
) -> Result<U256> {
    let amount_in_with_fee = amount_in * fee_rate / U256::from(1000);
    let amount_out = (amount_in_with_fee * reserve_out) / (reserve_in + amount_in_with_fee);
    Ok(amount_out)
}

pub async fn execute_arbitrage(
    provider: Arc<Provider<Ws>>,
    contract: Arc<Contract<Provider<Ws>>>,
    wallet: Arc<LocalWallet>,
    token_in: H160,
    token_out: H160,
    amount_in: U256,
    direction: TradeDirections,
) -> Result<()> {
    let transaction_type = match direction {
        TradeDirections::UNISWAP => TransactionType::UniswapTrade,
        TradeDirections::SUSHISWAP => TransactionType::SushiswapTrade,
    };

    execute_transaction(
        provider,
        transaction_type,
        contract,
        wallet,
        token_in,
        token_out,
        amount_in
    )
    .await
}

async fn execute_transaction(
    provider: Arc<Provider<Ws>>,
    transaction_type: TransactionType,
    contract: Arc<Contract<Provider<Ws>>>,
    wallet: Arc<LocalWallet>,
    token_in: H160,
    token_out: H160,
    amount_in: U256,
) -> Result<()> {
    let gas_price = provider.get_gas_price().await?;
    let deadline = U256::from(Utc::now().timestamp() + 300);
    let slippage_bps = U256::from(100);  // 1% slippage

    let method_name = match transaction_type {
        TransactionType::UniswapTrade => "executeUniswapTrade",
        TransactionType::SushiswapTrade => "executeSushiswapTrade",
    };

    let path: [H160; 2] = [token_in, token_out];
    let router_address = UNISWAP_ROUTER.parse::<H160>()?;
    let router_abi: Abi = load_router_abi()?;

    let uniswap_router = Contract::new(
        router_address,
        router_abi,
          provider.clone()
        );

    let expected_out =  uniswap_router
        .method::<_, Vec<U256>>("getAmountsOut", (amount_in, path))?
        .call()
        .await
        .map_err(|e| anyhow!("Failed to get amount: {}", e))?;

    let expected_out = expected_out[1];
    let min_out = calculate_minout(expected_out, slippage_bps);


    let mut tx = contract
        .method::<_, ()>(
            method_name,
            (token_in, token_out, amount_in, min_out, deadline),
        )?
        .gas_price(gas_price)
        .from(wallet.address())
        .tx;

    let gas_estimate = gas_estimate(&tx, provider.clone()).await?;
    let tx = tx.set_gas(gas_estimate);

    let signed_tx = wallet.sign_transaction(tx).await?;
    let raw_tx = tx.rlp_signed(&signed_tx);

    provider
        .send_raw_transaction(raw_tx)
        .await
        .map_err(|e| anyhow!("Failed to send transaction: {:?}", e))?;

    Ok(())
}


pub async fn monitor_mempool(
    provider: Arc<Provider<Ws>>,
    contract: Arc<Contract<Provider<Ws>>>,
    wallet: Arc<LocalWallet>,
    target_token_in: H160,
    target_token_out: H160,
) {
    let mut stream = match provider.subscribe_pending_txs().await {
        Ok(stream) => stream,
        Err(e) => {
            error!("❌ Failed to subscribe to pending transactions: {:?}", e);
            return;
        }
    };

    while let Some(tx_hash) = stream.next().await {
        let provider = provider.clone();
        let contract = contract.clone();
        let wallet = wallet.clone();

        tokio::spawn(async move {
            if let Ok(Some(tx)) = provider.get_transaction(tx_hash).await {
                if is_target_pair(&tx, target_token_in, target_token_out).await {
                    if let Ok((token_in, token_out, amount_in)) = decode_transaction(&tx).await {
                        match check_price_discrepancy(
                            provider.clone(),
                            token_in,
                            token_out,
                            amount_in,
                            contract.clone(),
                        )
                        .await
                        {
                            Ok(Some((use_sushiswap, profit))) => {
                                let contract_balance = check_contract_balance(
                                    provider.clone(),
                                    contract.address(),
                                    token_in,
                                )
                                .await
                                .unwrap_or(U256::zero());

                                if contract_balance >= amount_in {
                                    let direction = if use_sushiswap {
                                        TradeDirections::SUSHISWAP
                                    } else {
                                        TradeDirections::UNISWAP
                                    };

                                    if let Err(e) = execute_arbitrage(
                                        provider.clone(),
                                        contract.clone(),
                                        wallet.clone(),
                                        token_in,
                                        token_out,
                                        amount_in,
                                        direction,
                                    )
                                    .await
                                    {
                                        error!("⚠️ Failed to execute arbitrage: {:?}", e);
                                    } else {
                                        info!(
                                            "✅ Arbitrage executed: {} -> {} (amount: {}, profit: {})",
                                            token_in, token_out, amount_in, profit
                                        );
                                    }
                                } else {
                                    warn!(
                                        "⚠️ Insufficient balance for arbitrage: {} -> {} (amount: {})",
                                        token_in, token_out, amount_in
                                    );
                                }
                            }
                            Ok(None) => {
                                info!(
                                    "🔍 No arbitrage opportunity found: {} -> {} (amount: {})",
                                    token_in, token_out, amount_in
                                );
                            }
                            Err(e) => {
                                error!("❌ Error checking price discrepancy: {:?}", e);
                            }
                        }
                    }
                }
            } else {
                warn!("⚠️ Failed to get transaction: {:?}", tx_hash);
            }
        });
    }
}

pub async fn check_price_discrepancy(
    provider: Arc<Provider<Ws>>,
    token_in: H160,
    token_out: H160,
    amount_in: U256,
    contract: Arc<Contract<Provider<Ws>>>,
) -> Result<Option<(bool, U256)>> {
    let fee_uniswap = U256::from(997); // 0.3% fee
    let fee_sushiswap = U256::from(998); // 0.25% fee

    let (uni_reserve_in, uni_reserve_out) =
        simulate_transaction(provider.clone(), false, token_in, token_out).await?;
    let (sushi_reserve_in, sushi_reserve_out) =
        simulate_transaction(provider.clone(), true, token_in, token_out).await?;

    let uni_price = simulate_trade_profit(uni_reserve_in, uni_reserve_out, amount_in, fee_uniswap)?;
    let sushi_price = simulate_trade_profit(sushi_reserve_in, sushi_reserve_out, amount_in, fee_sushiswap)?;

    let (use_sushiswap, profit) = if sushi_price > uni_price {
        (true, sushi_price - uni_price)
    } else if uni_price > sushi_price {
        (false, uni_price - sushi_price)
    } else {
        return Ok(None);
    };

    let method_name = if use_sushiswap {
        "executeSushiswapTrade"
    } else {
        "executeUniswapTrade"
    };

    let tx = contract.method::<_, ()>(
        method_name,
        (token_in, token_out, amount_in, 3000, U256::one(), U256::from(Utc::now().timestamp() + 300)),
    )?;

    let gas_cost = gas_estimate(&tx.tx, provider).await?;
    let net_profit = profit - gas_cost;

    if net_profit > U256::exp10(15) {
        Ok(Some((use_sushiswap, net_profit)))
    } else {
        Ok(None)
    }
}