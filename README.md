# Vendimint

A rust crate that provides a simple API for building applications that need to receive payments over the bitcoin lightning network, but don't ever need to actually hold the funds. Use-cases include a vending machine, point-of-sale device, or any other application where a payment must be made to trigger some sort of action or notification, but the funds can/should be routed to another device. The benefit of this for vending machines or point-of-sale devices is that these devices can request payments and be informed of payment completion, but have no risk of fund losses through device theft/destruction. Furthermore, this crate can be used without hosting any dedicated infrastructure. This is discussed further in the [Architecture](#architecture) section.

## Project Structure

This workspace contains the following crates:

- **fedimint-cli** - Command-line interface for interacting with Fedimint (used by `vendimint-tests`)
- **fedimintd** - Fedimint daemon (used by `vendimint-tests`)
- **vendimint** - Published crate for remote receive vending/point-of-sale functionality
- **vendimint-tests** - Devimint integration tests for vendimint

## Development Setup

### Prerequisites

This project uses Nix for development environment setup. First, install Nix using the Determinate Systems installer:

```bash
curl --proto '=https' --tlsv1.2 -sSf -L https://install.determinate.systems/nix | sh -s -- install
```

### Getting Started

1. Clone the repository
2. Enter the Nix development shell:

```bash
nix develop
```

3. Once in the Nix shell, you can use standard Cargo commands:

```bash
# Build the entire workspace
cargo build

# Run tests
cargo test

# Run Clippy for linting
cargo clippy
```

### Running Integration Tests

To run the full protocol integration tests (devimint tests), use the provided script:

```bash
sh ./scripts/tests/protocol-tests.sh
```

**Important Note**: The CI system doesn't use Nix yet and doesn't run the `protocol-tests.sh` script. Please run this script locally to ensure all tests are passing before submitting changes. Sadly, the test is a bit flaky right now, so you may need to retry a few times. If it passes once for a given commit, that's good enough.

## Architecture

The vendimint API provides just three exported types:

- [`Machine`](https://docs.rs/vendimint/latest/vendimint/struct.Machine.html)
- [`MachineConfig`](https://docs.rs/vendimint/latest/vendimint/struct.MachineConfig.html)
- [`Manager`](https://docs.rs/vendimint/latest/vendimint/struct.Manager.html)

A machine refers to a device/application that receives funds, such as a vending machine or point-of-sale device. A manager refers to a device/application that manages one or more machines and is able to sweep funds received by them. A machine config is a struct that can be set by a manager for each machine, which includes details required for a machine to start receiving payments.

As the name suggests, vendimint uses [Fedimint](https://fedimint.org/) to process payments. It uses [Iroh](https://iroh.computer/) for the peer-to-peer networking layer between machines and managers. By relying on the user's federation of choice, and Iroh's hosted relays, vendimint is able to operate without the need for the user hosting any of their own infrastructure.

TODO: Flesh out the documentation of the crate's architecture.

## Contributing

1. Ensure you're working within the Nix development shell (`nix develop`)
2. Run the full test suite including integration tests before submitting changes
3. Follow the existing code style and linting rules
4. All code must pass `cargo clippy` with the workspace's strict linting configuration
