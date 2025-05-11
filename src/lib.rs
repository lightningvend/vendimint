#![warn(clippy::pedantic, clippy::nursery)]
#![allow(clippy::missing_const_for_fn)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::missing_panics_doc)]
#![allow(clippy::option_if_let_else)]

pub mod machine;
pub mod manager;
mod shared;

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    use fedimint_core::{Amount, config::FederationId};
    use fedimint_lnv2_common::contracts::{IncomingContract, PaymentImage};
    use iroh_docs::rpc::{AddrInfoOptions, client::docs::ShareMode};
    use machine::MachineProtocol;
    use tpe::{AggregatePublicKey, G1Affine};

    #[tokio::test]
    async fn test_machine_protocol() -> anyhow::Result<()> {
        let machine_storage_path = tempfile::tempdir()?;
        let machine_protocol =
            MachineProtocol::new(machine_storage_path.path().to_path_buf()).await?;

        let manager_storage_path = tempfile::tempdir()?;
        let manager_protocol =
            manager::ManagerProtocol::new(manager_storage_path.path().to_path_buf()).await?;

        let pk = fedimint_core::secp256k1::PublicKey::from_str(
            "03e7798ad2ded4e6dbc6a5a6a891dcb577dadf96842fe500ac46ed5f623aa9042b",
        )?;

        let federation_id = FederationId::dummy();

        let incoming_contract = IncomingContract::new(
            AggregatePublicKey(G1Affine::identity()),
            [127; 32],
            [255; 32],
            PaymentImage::Point(pk),
            Amount { msats: 1234 },
            5678,
            pk,
            pk,
            pk,
        );

        machine_protocol
            .write_payment_to_machine_doc(&federation_id, &incoming_contract)
            .await?;

        let machine_doc_ticket = machine_protocol
            .share_machine_doc(ShareMode::Write, AddrInfoOptions::Id)
            .await?;

        manager_protocol.add_machine(machine_doc_ticket).await?;

        Ok(())
    }
}
