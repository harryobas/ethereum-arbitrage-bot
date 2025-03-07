use ethers::{
    abi::Abi,
    contract:: Contract,
    providers::{Middleware, Provider, Ws},
    signers::{LocalWallet, Signer},
    types::{transaction::eip2718::TypedTransaction, BlockNumber, Eip1559TransactionRequest, H160, U256, U64},
};
use std::sync::Arc;
use chrono::Utc;
use anyhow::{Result, anyhow};
use futures_util::stream::StreamExt;
use log::{info, error};

use crate::constants::{SUSHISWAP_FACTORY_ADDRESS, UNISWAP_V2_FACTORY_ADDRESS};
use crate::utils::*;

pub enum TradeDirections {
    UNISWAP,
    SUSHISWAP,
}


pub fn load_contract_abi() -> Result<Abi> {
    Ok(CONTRACT_ABI.clone())
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

pub fn simulate_trade_profit(
    reserve_in: U256,
    reserve_out: U256,
    amount_in: U256,
    fee_rate: U256,
) -> Result<U256> {
    // Validate inputs
    if reserve_in.is_zero() || reserve_out.is_zero() {
        return Err(anyhow!("Reserves cannot be zero"));
    }
    if amount_in.is_zero() {
        return Err(anyhow!("Input amount cannot be zero"));
    }
    if fee_rate > U256::from(1000) || fee_rate.is_zero() {
        return Err(anyhow!("Invalid fee rate"));
    }

    // Calculate the input amount after fees
    let amount_in_with_fee = amount_in
        .checked_mul(fee_rate)
        .ok_or(anyhow!("Overflow in fee calculation"))?
        .checked_div(U256::from(1000))
        .ok_or(anyhow!("Division by zero in fee calculation"))?;

    // Calculate the output amount
    let numerator = amount_in_with_fee
        .checked_mul(reserve_out)
        .ok_or(anyhow!("Overflow in numerator calculation"))?;
    let denominator = reserve_in
        .checked_add(amount_in_with_fee)
        .ok_or(anyhow!("Overflow in denominator calculation"))?;

    let amount_out = numerator
        .checked_div(denominator)
        .ok_or(anyhow!("Division by zero in output calculation"))?;

    Ok(amount_out)
}


async fn execute_arbitrage(
    provider: Arc<Provider<Ws>>,
    contract: Arc<Contract<Provider<Ws>>>,
    wallet: Arc<LocalWallet>,
    token_in: H160,
    token_out: H160,
    amount_in: U256,
    direction: TradeDirections,
) -> Result<()> {
    let method_name = "startArbitrage";
    //let gas_price = provider.get_gas_price().await?;
    let deadline = U256::from(Utc::now().timestamp() + 300);

    let block = provider
        .get_block(BlockNumber::Latest)
        .await?
        .ok_or(anyhow!("Failed to fetch latest block"))?;
    let base_fee = block.base_fee_per_gas.ok_or(anyhow!("Base fee not available"))?;

    let max_priority_per_gas = base_fee
        .checked_div(U256::from(10))
        .unwrap_or(U256::from(2_000_000_000));

    let max_fee_per_gas = base_fee + max_priority_per_gas;

    let method = contract.method::<_, ()>(
        method_name,
        (token_in, amount_in, token_out, deadline, matches!(direction, TradeDirections::UNISWAP)),
    )?;

    let calldata = method.calldata().ok_or(anyhow!("Calldata not available"))?;

    let chain_id_u256: U256 = provider.get_chainid().await?;
    let chain_id_u64: U64 = if chain_id_u256 > U256::from(u64::MAX) {
        return Err(anyhow!("Chain ID is too large for U64"));
    } else {
        U64::from(chain_id_u256.as_u64())
    };

    let tx = Eip1559TransactionRequest {
        to: Some(contract.address().into()),
        data: Some(calldata),
        gas: None,
        max_fee_per_gas: Some(max_fee_per_gas),
        max_priority_fee_per_gas: Some(max_priority_per_gas),
        value: None,
        nonce: None,
        chain_id: Some(chain_id_u64),
        ..Default::default()
    };
    
    let typed_tx = TypedTransaction::Eip1559(tx.clone());
    let gas_estimate = provider.estimate_gas(&typed_tx, None).await?;

    let tx = Eip1559TransactionRequest {
        gas: Some(gas_estimate),
        ..tx
    };

    let signed_tx = wallet.sign_transaction(&TypedTransaction::Eip1559(tx.clone())).await?;
    let raw_tx = tx.rlp_signed(&signed_tx);

    let pending_tx = provider
        .send_raw_transaction(raw_tx)
        .await
        .map_err(|e| anyhow!("Failed to send transaction: {:?}", e))?;

    let receipt = pending_tx.await?;
    match receipt {
        Some(receipt) => {
            info!("✅  Transaction mined: {:?}", receipt.transaction_hash);
            info!("Gas used {}", receipt.gas_used.unwrap_or_default()); 
            info!("Max fee per gas {}", tx.max_fee_per_gas.unwrap_or_default());
            info!("Max priority fee per gas {}", tx.max_priority_fee_per_gas.unwrap_or_default());   
        }

        None => {
            error!("❌ Failed to mine transaction");
        }
    }
        

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
                        if let Ok(Some((use_sushiswap, profit))) = check_price_discrepancy(provider.clone(), token_in, token_out, amount_in).await {
                            let direction = if use_sushiswap {
                                TradeDirections::SUSHISWAP
                            } else {
                                TradeDirections::UNISWAP
                            };

                            if let Err(e) = execute_arbitrage(provider.clone(), contract.clone(), wallet.clone(), token_in, token_out, amount_in, direction).await {
                                error!("⚠️ Failed to execute arbitrage: {:?}", e);
                            } else {
                                info!(
                                    "✅ Arbitrage executed: {} -> {} (amount: {}, profit: {})",
                                    token_in, token_out, amount_in, profit
                                );
                            }
                        }
                    }
                }
            }
        });
    }
}


pub async fn check_price_discrepancy(
    provider: Arc<Provider<Ws>>,
    token_in: H160,
    token_out: H160,
    amount_in: U256,
) -> Result<Option<(bool, U256)>> {
    // Fees: 0.3% for Uniswap (997/1000), 0.25% for Sushiswap (998/1000)
    let fee_uniswap = U256::from(997);
    let fee_sushiswap = U256::from(998);

    // Simulate reserves on Uniswap and Sushiswap
    let (uni_reserve_in, uni_reserve_out) =
        simulate_transaction(provider.clone(), false, token_in, token_out).await?;
    let (sushi_reserve_in, sushi_reserve_out) =
        simulate_transaction(provider.clone(), true, token_in, token_out).await?;

    // Simulate trade output on Uniswap and Sushiswap
    let uni_output = simulate_trade_profit(uni_reserve_in, uni_reserve_out, amount_in, fee_uniswap)?;
    let sushi_output = simulate_trade_profit(sushi_reserve_in, sushi_reserve_out, amount_in, fee_sushiswap)?;

    // Determine which DEX is cheaper to buy from and which is more expensive to sell on
    let (buy_on_sushiswap, profit) = if sushi_output > uni_output {
        // Sushiswap is cheaper to buy from, Uniswap is more expensive to sell on
        (true, sushi_output - uni_output)
    } else if uni_output > sushi_output {
        // Uniswap is cheaper to buy from, Sushiswap is more expensive to sell on
        (false, uni_output - sushi_output)
    } else {
        // No price discrepancy
        return Ok(None);
    };

    // Check if the profit exceeds the threshold (1e15 wei = 0.001 ETH)
    let profit_threshold  = U256::exp10(15);
    if profit > profit_threshold {
        Ok(Some((buy_on_sushiswap, profit)))
    } else {
        Ok(None)
    }
}