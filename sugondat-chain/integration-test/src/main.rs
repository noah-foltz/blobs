use subxt::{
    utils::{AccountId32, MultiAddress},
    OnlineClient, PolkadotConfig,
};
use subxt_signer::sr25519::dev::{self};

#[subxt::subxt(runtime_metadata_path = "sugondat-metadata.scale")]
pub mod sugondat {}

#[tokio::main]
pub async fn main() {
    if let Err(err) = test_max_blob_size_exceeded().await {
        eprintln!("{err}");
    }
}

async fn test_max_blob_size_exceeded() -> Result<(), Box<dyn std::error::Error>> {
    // (the port 9988 is specified in the testnet.toml)
    let api = OnlineClient::<PolkadotConfig>::from_url("ws://127.0.0.1:9988").await?;
    println!("Connection with sugondat established.");

    let alice = dev::alice();

    let constant_query = sugondat::constants().blobs().max_blob_size();
    let max_blob_size = api.constants().at(&constant_query)? as usize;

    // create a collection with id `12`
    let submit_blob_tx = sugondat::tx()
        .blobs()
        .submit_blob(0, vec![0; max_blob_size]);

    let tx_progress = api
        .tx()
        .sign_and_submit_then_watch_default(&submit_blob_tx, &alice)
        .await;

    // That's weird, it immediately returns an error.
    // It seems like the `submit_extrinsic` function immediately applies the `validate_transaction` method.
    println!("{tx_progress:?}");

    Ok(())
}
