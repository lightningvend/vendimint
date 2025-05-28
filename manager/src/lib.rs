use std::path::{Path, PathBuf};
use tokio::sync::oneshot;

use fedimint_core::{config::FederationId, db::DatabaseValue};
use fedimint_core::bitcoin::hashes::Hash as BitcoinHash;
use fedimint_lnv2_common::{contracts::IncomingContract, ContractId};
use fedimint_core::bitcoin::hashes::sha256;
use iroh::{NodeAddr, protocol::Router};
use iroh_blobs::net_protocol::Blobs;
use iroh_docs::{Capability, DocTicket, protocol::Docs, store::Query};
use uuid::Uuid;

use shared::{
    CLAIM_ALPN, FEDERATION_INVITE_CODE_KEY, MachineConfig, SharedProtocol,
    claim_pin_from_keying_material,
};

const MACHINE_DOC_TICKETS_SUBDIR: &str = "machine_doc_tickets";

pub struct ManagerProtocol {
    router: Router,
    blobs: Blobs<iroh_blobs::store::fs::Store>,
    docs: Docs<iroh_blobs::store::fs::Store>,
    app_storage_path: PathBuf,
}

impl ManagerProtocol {
    pub async fn new(storage_path: &Path) -> anyhow::Result<Self> {
        let shared_protocol = SharedProtocol::new(storage_path).await?;

        let manager_protocol = Self {
            router: shared_protocol.router_builder.spawn(),
            blobs: shared_protocol.blobs,
            docs: shared_protocol.docs,
            app_storage_path: shared_protocol.app_storage_path,
        };

        // Ensure the machine doc tickets directory exists.
        let machine_doc_tickets_path = manager_protocol.get_machine_doc_ticket_path();
        std::fs::create_dir_all(&machine_doc_tickets_path).unwrap();

        Ok(manager_protocol)
    }

    pub async fn shutdown(&self) -> anyhow::Result<()> {
        self.router.shutdown().await
    }

    #[must_use]
    pub fn is_shutdown(&self) -> bool {
        self.router.is_shutdown()
    }

    pub async fn claim_machine(
        &self,
        node_addr: NodeAddr,
    ) -> anyhow::Result<(u32, oneshot::Sender<bool>)> {
        let conn = self
            .router
            .endpoint()
            .connect(node_addr, CLAIM_ALPN)
            .await?;

        let pin = claim_pin_from_keying_material(&conn);
        let (tx, rx) = oneshot::channel();
        let docs = self.docs.clone();
        let machine_doc_ticket_path = self.get_machine_doc_ticket_path();
        tokio::spawn(async move {
            if rx.await.unwrap_or(false) {
                if let Ok(mut recv) = conn.accept_uni().await {
                    if let Ok(bytes) = recv.read_to_end(1024 * 1024).await {
                        if let Ok(machine_doc_ticket) = serde_json::from_slice::<DocTicket>(&bytes)
                        {
                            if matches!(machine_doc_ticket.capability, Capability::Write(_)) {
                                let _ = docs.client().import(machine_doc_ticket.clone()).await;
                                let machine_id = Uuid::new_v4();
                                let _ = std::fs::write(
                                    machine_doc_ticket_path.join(machine_id.to_string()),
                                    serde_json::to_string(&machine_doc_ticket).unwrap(),
                                );
                            }
                        }
                    }
                }
            }
            conn.close(0u32.into(), b"done");
        });

        Ok((pin, tx))
    }

    pub fn get_machine(&self, machine_id: &Uuid) -> anyhow::Result<DocTicket> {
        let machine_doc_ticket_path = self
            .get_machine_doc_ticket_path()
            .join(machine_id.to_string());
        let machine_doc_ticket_str = std::fs::read_to_string(&machine_doc_ticket_path)?;
        let machine_doc_ticket: DocTicket = serde_json::from_str(&machine_doc_ticket_str)?;
        Ok(machine_doc_ticket)
    }

    pub fn list_machines(&self) -> anyhow::Result<Vec<(Uuid, DocTicket)>> {
        let machine_doc_tickets_path = self.get_machine_doc_ticket_path();
        let mut machines = Vec::new();

        for entry in std::fs::read_dir(machine_doc_tickets_path)? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                let file_name = entry.file_name().into_string().unwrap();
                let machine_id = Uuid::parse_str(&file_name).unwrap();
                let machine_doc_ticket_str = std::fs::read_to_string(entry.path())?;
                let machine_doc_ticket: DocTicket =
                    serde_json::from_str(&machine_doc_ticket_str).unwrap();
                machines.push((machine_id, machine_doc_ticket));
            }
        }

        Ok(machines)
    }

    pub async fn set_machine_config(
        &self,
        machine_id: &Uuid,
        machine_config: &MachineConfig,
    ) -> anyhow::Result<()> {
        let machine_doc_ticket = self.get_machine(machine_id)?;
        let machine_doc = self
            .docs
            .client()
            .open(machine_doc_ticket.capability.id())
            .await?
            .unwrap();
        machine_doc
            .set_bytes(
                self.docs.client().authors().default().await?,
                FEDERATION_INVITE_CODE_KEY.to_bytes(),
                serde_json::to_vec(machine_config).unwrap(),
            )
            .await?;
        Ok(())
    }

    pub async fn list_claimable_payments_for_machine(
        &self,
        node_addr: NodeAddr,
    ) -> anyhow::Result<Vec<(FederationId, IncomingContract)>> {
        let mut payments = Vec::new();

        for (_id, ticket) in self.list_machines()? {
            let doc = self
                .docs
                .client()
                .open(ticket.capability.id())
                .await?
                .unwrap();
            let _ = doc.start_sync(vec![node_addr.clone()]).await;

            const PREFIX: [u8; 2] = [0x01, 0xFF];
            let query = Query::single_latest_per_key().key_prefix(PREFIX).build();
            let mut stream = doc.get_many(query).await?;

            use futures_lite::StreamExt;
            while let Some(entry) = stream.next().await {
                let entry = entry?;
                let key = entry.key();
                if key.len() != 66 || key[0..2] != PREFIX {
                    continue;
                }

                let mut fid_bytes = [0u8; 32];
                fid_bytes.copy_from_slice(&key[2..34]);
                let mut cid_bytes = [0u8; 32];
                cid_bytes.copy_from_slice(&key[34..66]);

                let bytes = match self
                    .blobs
                    .client()
                    .read_to_bytes(entry.content_hash())
                    .await
                {
                    Ok(b) => b,
                    Err(_) => continue,
                };
                let (incoming_contract, _) = bincode::serde::decode_from_slice(
                    &bytes,
                    bincode::config::standard(),
                )?;

                payments.push((
                    FederationId(sha256::Hash::from_byte_array(fid_bytes)),
                    incoming_contract,
                ));
            }
        }

        Ok(payments)
    }

    pub async fn remove_claimed_payment(
        &self,
        federation_id: FederationId,
        contract_id: ContractId,
    ) -> anyhow::Result<()> {
        const PREFIX: [u8; 2] = [0x01, 0xFF];
        let mut key = PREFIX.to_vec();
        key.extend_from_slice(federation_id.0.as_byte_array());
        key.extend_from_slice(contract_id.0.as_ref());

        for (_id, ticket) in self.list_machines()? {
            let doc = self
                .docs
                .client()
                .open(ticket.capability.id())
                .await?
                .unwrap();
            let _ = doc
                .del(self.docs.client().authors().default().await?, key.clone())
                .await?;
        }

        Ok(())
    }

    fn get_machine_doc_ticket_path(&self) -> PathBuf {
        self.app_storage_path.join(MACHINE_DOC_TICKETS_SUBDIR)
    }
}
