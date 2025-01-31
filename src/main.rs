 

mod arbitrage_services;
mod constants;

use ethers::{
    types::H160,
    contract::Contract,
    providers::{Ws, Provider},
    signers::LocalWallet,

};

use std::sync::Arc;

use constants::{CONTRACT_ADDRESS, WETH_ADDRESS};
use arbitrage_services::{load_contract_abi, monitor_mempool};
use clap::Parser;

#[derive(Parser)]
#[clap(version, author, about)]
struct Cli {
    #[clap(short = 'p', long)]
    private_key: String,

    #[clap(short = 'r', long)]
    rpc_url: String,

    #[clap(short = 't', long)]
    token_out: H160,
}


#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let provider = Arc::new(Provider::<Ws>::connect(cli.rpc_url)
        .await
        .expect("Failed to connect to WebSocket provider"));

    let abi = load_contract_abi().await.expect("Failed to load contract ABI");
    let contract_address = CONTRACT_ADDRESS.parse::<H160>()
        .expect("Invalid contract address");

    let contract = Arc::new(Contract::new(contract_address, abi, provider.clone()));

    let wallet = Arc::new(cli.private_key.parse::<LocalWallet>().expect("Invalid private key"));

    let target_token_in = WETH_ADDRESS.parse::<H160>().expect("Faild to parse token address");
    let target_token_out = cli.token_out;

    monitor_mempool(
        provider.clone(),
        contract.clone(),
        wallet.clone(), 
        target_token_in, 
        target_token_out
    ).await;


    
        
       

    

}
    
   
