use std::convert::Infallible;

use fedimint_core::fedimint_build_code_version_env;

#[tokio::main]
async fn main() -> anyhow::Result<Infallible> {
    fedimintd::run(
        fedimintd::default_modules(),
        fedimint_build_code_version_env!(),
        None,
    )
    .await
}
