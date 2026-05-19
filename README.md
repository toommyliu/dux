# dux

## Usage

```sh
cargo run -- ~/Downloads
cargo run -- ~/Downloads --list
cargo run -- ~/Downloads --list --all --filter iso --limit 20
cargo run -- ~/Downloads --json
cargo run -- ~/Downloads --json | jq '.root.children[] | {size, path}'
```
