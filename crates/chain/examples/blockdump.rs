//! Differential test of block parsing against xdagj: prints, for every
//! 512-byte block of a file, what xdagd's parser makes of it, in the format
//! of `tools/interop/BlockDump.java` (which prints what xdagj makes of it).
//!
//! `cargo run --release -p xdag-chain --example blockdump -- blocks.dat xdagd.txt`

use std::io::{BufWriter, Read, Write};

use xdag_types::difficulty::hash_difficulty;
use xdag_types::{Block, Link, LinkTarget, PublicKey};

fn links(ls: &[Link]) -> String {
    let items: Vec<String> = ls
        .iter()
        .map(|l| {
            let target = match l.target {
                LinkTarget::Block(h) => hex::encode(h.0),
                LinkTarget::Address(a) => hex::encode(a.0),
            };
            // the parser has already rejected amounts xdagj cannot represent
            format!("{}:{}:{}", l.kind.nibble(), target, l.amount.to_nano_legacy().map(|n| n.0).unwrap_or(0))
        })
        .collect();
    format!("[{}]", items.join(","))
}

fn keys(ks: &[PublicKey]) -> String {
    format!("[{}]", ks.iter().map(|k| hex::encode(k.serialize())).collect::<Vec<_>>().join(","))
}

fn line(raw: &[u8]) -> String {
    let Ok(b) = Block::parse(raw) else { return "unparseable".into() };
    let opt = |v: Option<String>| v.unwrap_or_else(|| "-".into());
    let insigs: Vec<String> = b.insigs.iter().map(|(_, i)| i.to_string()).collect();
    format!(
        "hash={} time={:x} type={:x} fee={} in={} out={} txnonce={} remark={} keys={} insigs=[{}] outsig={} nonce={} outsigindex={} verified={} diff={:x}",
        hex::encode(b.hash().0),
        b.time,
        b.type_word,
        b.header_fee,
        links(&b.inputs),
        links(&b.outputs),
        opt(b.tx_nonce.map(|n| n.to_string())),
        opt(b.remark.map(hex::encode)),
        keys(&b.pubkeys),
        insigs.join(", "),
        opt(b.outsig.map(|s| hex::encode(s.r) + &hex::encode(s.s))),
        opt(b.nonce.map(hex::encode)),
        b.outsig_index(),
        b.verified_keys().map(|k| keys(&k)).unwrap_or_else(|_| "error".into()),
        hash_difficulty(&b.hash().0),
    )
}

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!("usage: blockdump <blocks.dat> <out.txt>");
        std::process::exit(2);
    }
    let mut input = std::io::BufReader::new(std::fs::File::open(&args[1])?);
    let mut out = BufWriter::new(std::fs::File::create(&args[2])?);
    let mut raw = [0u8; 512];
    let mut n = 0u64;
    while input.read_exact(&mut raw).is_ok() {
        writeln!(out, "{n} {}", line(&raw))?;
        n += 1;
    }
    out.flush()?;
    println!("{n} blocks");
    Ok(())
}
