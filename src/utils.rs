use ethers::{ 
    abi::Abi,
    types::{
        Transaction,
        transaction::eip2718::TypedTransaction,
         H160, 
         U256
        }, contract::Contract, providers::{Middleware, Provider, Ws}
};
use anyhow::{Result, anyhow};
use std::sync::Arc;
use lazy_static::lazy_static;

lazy_static! {
    pub static ref UNISWAP_V2_ROUTER_ABI: Abi = serde_json::from_str(include_str!("../UniswapV2RouterABI.json")).unwrap();
    pub static ref POOL_ABI: Abi = serde_json::from_str(include_str!("../UniswapV2PairABI.json")).unwrap();
    pub static ref FACTORY_ABI: Abi = serde_json::from_str(include_str!("../UniswapV2FactoryABI.json")).unwrap();
    pub static ref CONTRACT_ABI: Abi = serde_json::from_str(include_str!("../ContractABI.json")).unwrap();
}


pub async fn get_pool_address(
    provider: Arc<Provider<Ws>>,
    factory_address: H160,
    token_in: H160,
    token_out: H160,
) -> Result<H160> {
    // Create a contract instance for the factory
    let factory = Contract::new(factory_address, FACTORY_ABI.clone(), provider.clone());

    // Ensure tokens are in canonical order (lower address first)
    let (token0, token1) = if token_in < token_out {
        (token_in, token_out)
    } else {
        (token_out, token_in)
    };

    // Call the `getPair` method to get the pool address
    let pair_address: H160 = factory
        .method("getPair", (token0, token1))?
        .call()
        .await
        .map_err(|e| {
            anyhow!(
                "Failed to query pair address for tokens {} and {} from factory {}: {}",
                token0,
                token1,
                factory_address,
                e
            )
        })?;

    // Check if the pool exists
    if pair_address == H160::zero() {
        return Err(anyhow!(
            "No pool found for tokens {} and {} on factory {}",
            token0,
            token1,
            factory_address
        ));
    }

    Ok(pair_address)
}


pub async fn get_gas_estimate(tx: &TypedTransaction, provider: Arc<Provider<Ws>>) -> Result<U256> {
    provider
        .estimate_gas(tx, None)
        .await
        .map_err(|e| anyhow!("Failed to get gas estimate: {:?}", e))
}

pub async fn is_target_pair(tx: &Transaction, target_token_in: H160, target_token_out: H160) -> bool {
    let decoded_tx = decode_transaction(tx).await;
    match decoded_tx {
        Ok((token_in, token_out, _)) => {
            let is_match = (token_in == target_token_in && token_out == target_token_out)
                || (token_in == target_token_out && token_out == target_token_in);
            if !is_match {
                log::info!("Transaction does not involve the target token pair: {:?}", tx.hash);
            }
            is_match
        }
        Err(e) => {
            log::info!("Failed to decode transaction: {:?}", e);
            false
        }
    }
}
pub async fn decode_transaction(tx: &Transaction) -> Result<(H160, H160, U256)> {
    let func = UNISWAP_V2_ROUTER_ABI
        .function("swapExactTokensForTokens")
        .map_err(|e| anyhow!("Failed to load UniswapV2Router function: {:?}", e))?;

    let decoded = func
        .decode_input(&tx.input)
        .map_err(|e| anyhow!("Failed to decode transaction: {:?}", e))?;

    // Extract amountIn (U256)
    let amount_in = decoded[0]
        .clone()
        .into_uint()
        .ok_or(anyhow!("Error decoding amount_in"))?;

    // Extract path (Vec<H160>)
    let path = decoded[2]
        .clone()
        .into_array()
        .ok_or(anyhow!("Error decoding path"))?;

    // Extract token_in and token_out from the path
    let token_in = path[0]
        .clone()
        .into_address()
        .ok_or(anyhow!("Error decoding token_in"))?;
    let token_out = path[1]
        .clone()
        .into_address()
        .ok_or(anyhow!("Error decoding token_out"))?;

    Ok((token_in, token_out, amount_in))
}


