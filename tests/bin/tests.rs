use devimint::cmd;
use devimint::devfed::DevJitFed;
use devimint::federation::Client;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    println!("Starting devimint test...");

    devimint::run_devfed_test()
        .call(|dev_fed, _process_mgr| async move {
            let fed = dev_fed.fed().await?;

            fed.pegin_gateways(
                1_000_000,
                vec![
                    dev_fed.gw_lnd().await.unwrap(),
                    dev_fed.gw_ldk().await.unwrap(),
                ],
            )
            .await?;

            let ldk_gw_addr = dev_fed.gw_ldk().await.unwrap().addr.clone();

            println!("Successfully completed devimint test!");

            Ok(())
        })
        .await
}
