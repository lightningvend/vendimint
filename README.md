# Vendimint

A rust crate that provides a simple API for building applications that need to receive payments over the bitcoin lightning network, but don't ever need to actually hold the funds. Use-cases include a vending machine, point-of-sale device, or any other application where a payment must be made to trigger some sort of action or notification, but the funds can/should be routed to another device. The benefit of this for vending machines or point-of-sale devices is that these devices can request payments and be informed of payment completion, but have no risk of fund losses through device theft/destruction. Furthermore, this crate can be used without hosting any dedicated infrastructure. This is discussed further in the [Architecture](#architecture) section.

## Architecture

TODO: Flesh out the documentation of the crate's architecture.

The vendimint API is fairly simple. It provides two different device types, machines and managers. A machine is a device/application that receives funds, such as a vending machine or point-of-sale device. A manager is a device/application that manages one or more machines and is able to sweep funds received by them. Machines begin as unclaimed and unconfigured. They must be claimed by a manager, at which point the manager will automatically configure them, which gives them the details required to start receiving payments.

Vendimint uses [Fedimint](https://fedimint.org/) to process payments (as the name suggests), and [Iroh](https://iroh.computer/) for the peer-to-peer networking layer between machines and managers. By relying on the uptime of the user's federation of choice, and of Iroh's hosted relays, vendimint machines and managers are able to operate without the need for any additional hosted infrastructure.

### How Payments Work

As mentioned above, [Fedimint](https://fedimint.org/) is used as the underlying lightning payment processor. Fedimint's lightning module allows for funds to be locked into a contract that, once funded via a lightning gateway, can only be redeemed by possessing a private key that is specified at contract creation.

1. An unfunded lightning receive contract is created in the federation, and a `claim_pk` is specified (along with some other data that isn't relevant here)
2. The contract is funded by a lightning gateway in exchange for the invoice's pre-image, which is decrypted by the federation
3. Anyone with the secret key corresponding to `claim_pk` can now claim the funds from the contract

For normal lightning receives, steps 1 and 3 are performed by the same device. In vendimint, step 1 is performed by a machine and step 3 is performed by its manager.

### Key-Value Store Interface

Vendimint provides a shared key-value store between machines and their managers for application-specific data exchange. The KV interface includes:

- **`KvEntry`** - Represents a key-value pair with metadata including the author (which device wrote it) and timestamp
- **`KvEntryAuthor`** - Enum indicating whether an entry was written by the `Machine` or `Manager`

Both machines and managers can read and write arbitrary key-value data that automatically syncs between the paired devices whenever both are online. This enables use cases such as:

- Machines reporting status information to managers
- Managers sending configuration updates to machines
- Sharing application state between devices
- Implementing custom protocols on top of the vendimint transport layer

The KV store operates independently of payment processing, uses the same secure peer-to-peer networking layer, and can contain arbitrary binary data for keys and values. However, no guarantees are made as to the consistency of the KV store between a machine and its manager, or the order in which entries sync between them.

## Project Structure

This workspace contains the following crates:

- **fedimint-cli** - Command-line interface for interacting with Fedimint (used by `vendimint-tests`)
- **fedimintd** - Fedimint daemon (used by `vendimint-tests`)
- **vendimint** - Published crate for remote receive vending/point-of-sale functionality
- **vendimint-tests** - Devimint integration tests for vendimint

## Contributing

Contributing is pretty straightforward: Fork the repo and make a PR! If the PR gets a green check from CI, I'll take a look! If it gets a red check, take a look at the logs and iterate until it's green. All CI checks run inside a Nix shell, so any issues should be fully reproducible for you locally. In CI, commands are run in the following format:

```bash
nix develop --command foo
```

This simply runs a command within a Nix shell while the terminal itself isn't in a Nix shell. It is functionally the same as the following:

```bash
nix develop

# This is now run inside Nix.
foo
```

## Development Setup

### Prerequisites

This project uses Nix for development environment setup. If you haven't yet, install Nix using the Determinate Systems installer:

```bash
curl --proto '=https' --tlsv1.2 -sSf -L https://install.determinate.systems/nix | sh -s -- install
```

That's all you need to develop in this repo!

Note: Nix works on Unix-like platforms, which includes macOS and Linux, but not Windows. If you are on Windows, you can try using WSL but I've found that to be a pain. You can always use GitHub Codespaces if needed!

### Getting Started

1. Clone the repository
2. Enter the Nix development shell:

```bash
nix develop
```

This will take a while to run the first time you run it (~30 minutes from my experience). It should take only a few seconds every time after that. Developing inside of a Nix shell ensures that all dependencies and tools are available to you within the shell, including Rust and Cargo. Nix is the _only_ thing you need to install to get started developing.

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

I've found the test to be slightly flakey, so you may need to retry a few times. If it passes once for a given commit, the code is almost certainly fine.
