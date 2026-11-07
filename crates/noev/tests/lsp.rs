//! Talks to `noev lsp` as an editor would, over its standard input and output.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{Value, json};

struct Client {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
}

impl Client {
    fn start() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_noev"))
            .arg("lsp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("noev lsp starts");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut client = Client {
            child,
            stdin,
            stdout,
            next_id: 1,
        };
        let init = client.request(
            "initialize",
            json!({ "capabilities": { "general": { "positionEncodings": ["utf-16"] } } }),
        );
        assert_eq!(init["capabilities"]["hoverProvider"], json!(true));
        client.notify("initialized", json!({}));
        client
    }

    fn send(&mut self, message: Value) {
        let body = message.to_string();
        write!(self.stdin, "Content-Length: {}\r\n\r\n{body}", body.len()).unwrap();
        self.stdin.flush().unwrap();
    }

    fn receive(&mut self) -> Value {
        let mut length = 0;
        loop {
            let mut line = String::new();
            self.stdout.read_line(&mut line).unwrap();
            let line = line.trim();
            if line.is_empty() {
                break;
            }
            if let Some(n) = line.strip_prefix("Content-Length: ") {
                length = n.parse().unwrap();
            }
        }
        let mut body = vec![0; length];
        self.stdout.read_exact(&mut body).unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.send(json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    /// Sends a request and returns its result, skipping notifications that come first.
    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        loop {
            let message = self.receive();
            if message["id"] == json!(id) {
                return message["result"].clone();
            }
        }
    }

    /// The next diagnostics published.
    fn diagnostics(&mut self) -> Vec<Value> {
        loop {
            let message = self.receive();
            if message["method"] == "textDocument/publishDiagnostics" {
                return message["params"]["diagnostics"].as_array().unwrap().clone();
            }
        }
    }

    fn open(&mut self, uri: &str, text: &str) {
        self.notify(
            "textDocument/didOpen",
            json!({ "textDocument": { "uri": uri, "languageId": "noev", "version": 1, "text": text } }),
        );
    }

    fn at(&mut self, method: &str, uri: &str, line: u32, character: u32) -> Value {
        self.request(
            method,
            json!({ "textDocument": { "uri": uri }, "position": { "line": line, "character": character } }),
        )
    }

    fn stop(mut self) {
        self.request("shutdown", Value::Null);
        self.notify("exit", Value::Null);
        assert!(self.child.wait().unwrap().success());
    }
}

const PROGRAM: &str = "struct Experiment {
    angle: Float
    state: Qubit

    fn finish(mv self) -> Bit {
        mv self.state.measure()
    }
}

fn bell() -> (Bit, Bit) {
    let a = |+>
    let b = |0>
    ctrl a { b.x() }
    (mv a.measure(), mv b.measure())
}

fn main() {
    let e = Experiment { angle: 0.5, state: |0> }
    print(e.angle, mv e.finish())
    let d = sample 10 { bell() }
    print(d)
}
";

#[test]
fn diagnostics_follow_the_text() {
    let mut client = Client::start();
    let uri = "file:///tmp/bad.nv";
    client.open(uri, "fn main() {\n    let q = |0>\n}\n");
    let diagnostics = client.diagnostics();
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    assert_eq!(diagnostics[0]["code"], "N003");
    assert_eq!(diagnostics[0]["range"]["start"]["line"], 2);

    // Fixing it clears the error.
    client.notify(
        "textDocument/didChange",
        json!({
            "textDocument": { "uri": uri, "version": 2 },
            "contentChanges": [{ "text": "fn main() {\n    let q = |0>\n    mv q.discard()\n}\n" }],
        }),
    );
    assert!(client.diagnostics().is_empty());
    client.stop();
}

#[test]
fn hover_definition_and_symbols() {
    let mut client = Client::start();
    let uri = "file:///tmp/program.nv";
    client.open(uri, PROGRAM);
    assert!(client.diagnostics().is_empty());

    // Hover over `bell` where it is called: its signature.
    let hover = client.at("textDocument/hover", uri, 19, 25);
    let text = hover["contents"]["value"].as_str().unwrap();
    assert!(text.contains("fn bell() -> (Bit, Bit)"), "{text}");

    // Hover over a variable: its type.
    let hover = client.at("textDocument/hover", uri, 20, 10);
    let text = hover["contents"]["value"].as_str().unwrap();
    assert!(text.contains("d: Distribution<(Bit, Bit)>"), "{text}");

    // Go to the definition of the method `finish`, and of the field `angle`.
    let def = client.at("textDocument/definition", uri, 18, 25);
    assert_eq!(
        def["range"]["start"],
        json!({ "line": 4, "character": 7 }),
        "{def}"
    );
    let def = client.at("textDocument/definition", uri, 18, 13);
    assert_eq!(def["range"]["start"]["line"], 1, "{def}");

    let symbols = client.request(
        "textDocument/documentSymbol",
        json!({ "textDocument": { "uri": uri } }),
    );
    let names: Vec<&str> = symbols
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["Experiment", "bell", "main"]);
    client.stop();
}

const DOCUMENTED: &str = "/// Flips `q` when `flip` is 1.
fn correct(q: mut Qubit, flip: Bit) {
    if flip == 1 { q.x() }
}

fn bell() -> (Bit, Bit) {
    let a = |+>
    let b = |0>
    ctrl a { b.x() }
    let m = mv a.measure()
    correct(b, m)
    (m, mv b.measure())
}

fn main() {
    let d = sample 10 { bell() }
    print(d.count((0, 0)), Float::PI)
}
";

fn hover_text(client: &mut Client, uri: &str, line: u32, character: u32) -> String {
    let hover = client.at("textDocument/hover", uri, line, character);
    hover["contents"]["value"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

#[test]
fn hover_explains_functions_builtins_and_keywords() {
    let mut client = Client::start();
    let uri = "file:///tmp/documented.nv";
    client.open(uri, DOCUMENTED);
    assert!(client.diagnostics().is_empty());

    // A function: signature, doc comment, how it runs, whether it is reversible.
    let text = hover_text(&mut client, uri, 10, 5);
    assert!(
        text.contains("fn correct(q: mut Qubit, flip: Bit)"),
        "{text}"
    );
    assert!(text.contains("Flips `q` when `flip` is 1."), "{text}");
    assert!(text.contains("**Runs as** a circuit"), "{text}");
    assert!(text.contains("**Reversible** (inferred)"), "{text}");
    let text = hover_text(&mut client, uri, 5, 4);
    assert!(
        text.contains("**Not reversible**: it creates qubits"),
        "{text}"
    );

    // Built-ins: a gate, measure, a distribution method, a constant.
    assert!(hover_text(&mut client, uri, 8, 16).contains("Pauli X"));
    assert!(hover_text(&mut client, uri, 9, 18).contains("Measures the qubit in the Z basis"));
    assert!(hover_text(&mut client, uri, 16, 13).contains("How many runs gave `x`"));
    assert!(hover_text(&mut client, uri, 16, 33).contains("π = 3.14159"));

    // Keywords and literals.
    assert!(hover_text(&mut client, uri, 8, 4).contains("under quantum control"));
    assert!(hover_text(&mut client, uri, 9, 12).contains("Gives the value away"));
    assert!(hover_text(&mut client, uri, 15, 13).contains("Runs the block `n` times"));
    let text = hover_text(&mut client, uri, 6, 13);
    assert!(
        text.contains("|+>: Qubit") && text.contains("A fresh qubit: |+⟩"),
        "{text}"
    );

    // A variable holding qubits says it must be consumed.
    assert!(hover_text(&mut client, uri, 7, 8).contains("must be consumed exactly once"));
    client.stop();
}

const ENUMS: &str = "/// A measurement basis.
enum Basis {
    X
    Y

    fn flip(ref self) -> Self {
        match self {
            Basis::X => Basis::Y,
            Basis::Y => Basis::X,
        }
    }
}

fn main() {
    print(Basis::X.flip())
}
";

#[test]
fn enums_hover_definition_and_symbols() {
    let mut client = Client::start();
    let uri = "file:///tmp/enums.nv";
    client.open(uri, ENUMS);
    assert!(client.diagnostics().is_empty());

    // The enum, from a type annotation: its variants and methods, and its doc comment.
    let text = hover_text(&mut client, uri, 5, 26);
    assert!(
        text.contains("enum Basis {") && text.contains("fn Basis::flip(ref self) -> Basis"),
        "{text}"
    );
    assert!(text.contains("A measurement basis."), "{text}");
    // A variant in a pattern, and as a value.
    assert!(hover_text(&mut client, uri, 7, 18).contains("Basis::X"));
    assert!(hover_text(&mut client, uri, 7, 31).contains("Basis::Y"));
    assert!(hover_text(&mut client, uri, 6, 9).contains("Runs the first arm"));

    // Go to the variant `Y` from the value `Basis::Y`.
    let def = client.at("textDocument/definition", uri, 7, 31);
    assert_eq!(
        def["range"]["start"],
        json!({ "line": 3, "character": 4 }),
        "{def}"
    );

    let symbols = client.request(
        "textDocument/documentSymbol",
        json!({ "textDocument": { "uri": uri } }),
    );
    let basis = &symbols[0];
    assert_eq!(basis["name"], "Basis");
    let children: Vec<&str> = basis["children"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(children, ["X", "Y", "flip"]);
    client.stop();
}
