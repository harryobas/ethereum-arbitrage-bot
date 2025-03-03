 

mod arbitrage_services;
mod constants;
mod utils;

use ethers::{
    types::H160,
    contract::Contract,
    providers::{Ws, Provider},
    signers::LocalWallet,

};

use std::sync::Arc;
use std::env;
use dotenv::dotenv;

use constants::{CONTRACT_ADDRESS, DAI_ADDRESS, QUICKNODE_WS_URL, WETH_ADDRESS};
use arbitrage_services::{load_contract_abi, monitor_mempool};


#[tokio::main]
async fn main() {
    
    dotenv().ok();

    let provider = Provider::<Ws>::connect(QUICKNODE_WS_URL)
        .await
        .expect("Failed to connect to WebSocket provider");
    let provider = Arc::new(provider);

    let contract_abi = load_contract_abi().expect("Failed to load contract ABI");
    let contract_address = CONTRACT_ADDRESS.parse::<H160>()
        .expect("Invalid contract address");

    let contract = Arc::new(Contract::new(
        contract_address, 
        contract_abi, 
        provider.clone()
    )
);

    let private_key = env::var("PRIVATE_KEY").expect("missing private key");
    let wallet = Arc::new(
        private_key.parse::<LocalWallet>().expect("Invalid private key")
    );

    let target_token_in = DAI_ADDRESS.parse::<H160>().expect("Faild to parse token address");
    let target_token_out = WETH_ADDRESS.parse::<H160>().expect("Faild to parse token address");

    monitor_mempool(
        provider.clone(),
        contract.clone(),
        wallet.clone(), 
        target_token_in, 
        target_token_out
    ).await;


}
    
   
