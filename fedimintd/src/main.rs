use fedimint_core::fedimint_build_code_version_env;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    unsafe { std::env::set_var("FM_ENABLE_MODULE_LNV2_ENV", "true") };

    fedimintd::run(
        fedimintd::default_modules,
        fedimint_build_code_version_env!(),
        None,
    )
    .await
}
