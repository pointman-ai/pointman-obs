# hello: a Pointman extension template

A compiled extension in Rust: one command, `hello.greet`, that writes a greeting to a file and reports
it. Make a repo from this template named `pointman-<id>`, with the topic `pointman-extension`, rename
`hello` everywhere (Cargo.toml, the id in extension.toml, and the command, tool and event names), and
write your own commands in `src/main.rs`. It's MIT, as every extension repo is by default.

- **The SDK:** [pointman-extension](https://github.com/pointman-ai/pointman-extension) speaks the
  node's protocol over stdin and stdout, so `src/main.rs` only declares commands and answers them.
- **Run the tests:** `cargo test`. They start the built binary the way a node does, against the SDK's
  stand-in node (`testing::StandIn`), which also answers secrets within the manifest's permissions.
- **Try it on a machine:** `cargo build --release`, copy `target/release/hello` to `bin/hello`, then
  `pointman add .` from this folder, and run its command from any thread.
- **Release it:** bump `version` in extension.toml, then push the tag (`git tag v0.1.1 && git push
  origin v0.1.1`). `release.yml` builds macOS (Apple silicon and Intel) and Linux (x86_64 and arm64,
  static), attaches `hello-<version>-<platform>.tar.gz` for each, and core takes the release into the
  workspace's catalogue. Machines install it with `pointman add hello` and move to new releases with
  `pointman upgrade`.

An extension in Python is still possible (Pointman's `sdk/template`), for one that wraps a Python app's
own API, as Resolve and Blender do.
