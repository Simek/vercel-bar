# Vercel Bar

A macOS menu-bar app for monitoring Vercel teams and projects. View deployment status, recent deployments, analytics details. Open project subpages, or redeploy production with one click.

## Setup

Launch app, add a Vercel API token in **Settings**. 

## Contributing

Requires Rust and macOS with Xcode command-line tools.

```sh
cargo fmt-check
cargo check --all-targets
cargo lint
```

`cargo build-app` builds the release app, bundles it and moves to `/Applications/VercelBar.app`.

> [!note]
> For development, `VERCEL_TOKEN` can be set in the environment or a local `.env` file.
