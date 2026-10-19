//! The extension against a stand-in node (pointman-extension's testing::StandIn), started as a node
//! starts it: `cargo test` builds bin/hello and talks to it over stdin and stdout.

use pointman_extension::testing::StandIn;
use serde_json::json;

fn hello() -> StandIn {
    StandIn::new(env!("CARGO_MANIFEST_DIR")).binary(env!("CARGO_BIN_EXE_hello"))
}

#[test]
fn greet_writes_the_greeting() {
    let node = hello().settings(json!({"greeting": "Hi"})).start().unwrap();
    let done = node.run("hello.greet", json!({"name": "James"})).unwrap();
    assert_eq!(done.result, json!({"text": "Hi, James!"}));
    assert_eq!(std::fs::read_to_string(&done.outputs[0].path).unwrap(), "Hi, James!\n");
    assert_eq!(node.events()["hello.last"], json!({"name": "James"}));
}

#[test]
fn the_default_greeting() {
    let node = hello().start().unwrap();
    assert_eq!(node.run("hello.greet", json!({"name": "Ada"})).unwrap().result, json!({"text": "Hello, Ada!"}));
}

#[test]
fn a_missing_name_fails() {
    let node = hello().start().unwrap();
    let failed = node.run("hello.greet", json!({})).unwrap_err();
    assert_eq!(failed.message, "name is missing");
}
