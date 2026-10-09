# s1-mcp

MCP server for System One decision models (Clef, Kev, and any `/v1/systemone` endpoint).

Infrastructure scaffold; the Rust implementation will arrive through a PR to `dev`.

## Development

Enter the pinned Rust 1.95.0 environment with `nix develop` or `direnv allow`.
Run `nix flake check` for formatting, clippy, tests, and documentation checks.
Build the binary with `nix build .#default`.

## License

Apache-2.0.
