# Protocol

A Rust workspace project that integrates Fedimint federation protocols with Iroh networking capabilities. This project consists of multiple crates that work together to provide a comprehensive protocol implementation.

## Project Structure

This workspace contains the following crates:

- **fedimint-cli** - Command-line interface for interacting with Fedimint (used by `vendimint-tests`)
- **fedimintd** - Fedimint daemon implementation (used by `vendimint-tests`)
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

**Important Note**: The CI system doesn't use Nix yet and doesn't run the `protocol-tests.sh` script. Please run this script locally to ensure all tests are passing before submitting changes.

## Development Notes

- The project uses Rust 2024 edition
- Strict Clippy linting is enabled with nursery and pedantic rules
- Performance-critical cryptographic crates are optimized even in debug builds
- The project integrates with Fedimint version 0.7.2 and Iroh version 0.34.x

## Architecture

This project bridges Fedimint's federation protocols with Iroh's networking capabilities, providing:

- Fedimint daemon and client implementations
- Lightning Network integration
- Iroh-based peer-to-peer networking
- Shared utilities and common functionality
- Comprehensive testing infrastructure

## Contributing

1. Ensure you're working within the Nix development shell (`nix develop`)
2. Run the full test suite including integration tests before submitting changes
3. Follow the existing code style and linting rules
4. All code must pass `cargo clippy` with the workspace's strict linting configuration
