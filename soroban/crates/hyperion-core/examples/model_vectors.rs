//! Regenerate the common corpus: cargo run -p hyperion-core --example model_vectors -- ../testdata
use hyperion_core::model::{ChainClock, ModelRouter, Operation, Rail};
use std::{env, fmt::Write as _, fs, path::Path};

fn word(kind: u64, small: u64, arg: u64) -> u64 {
    // The arg bits contribute to the full `small` field; preserve the requested actor.
    let small = if (4..=8).contains(&kind) {
        (small + 3 - (arg * 256) % 3) % 3
    } else {
        small
    };
    kind | (small << 8) | (arg << 16)
}
fn main() {
    let root = env::args().nth(1).expect("output testdata directory");
    fs::create_dir_all(&root).unwrap();
    let mut words = vec![
        word(2, 0, 1),
        word(1, 0, 100_000),
        word(4, 1, 0),
        word(1, 5, 100_000),
        word(0, 0, 100_003),
        word(6, 0, 100),
        word(7, 0, 0),
        word(9, 0, 3600),
        word(7, 0, 0),
        word(9, 720, 169200),
        word(7, 0, 0),
        word(5, 1, 0),
        word(5, 0, 0),
        word(3, 0, 0),
        word(2, 0, 0),
        word(3, 0, 0),
        word(3, 0, 0),
        word(1, 0, 100_000),
        word(0, 3, 10_003),
        word(6, 0, 101),
        word(6, 0, 20),
        word(8, 1, 1),
        word(7, 0, 1),
        word(0, 0, 1_000_000_000),
        word(9, 720, 0),
        word(0, 0, 1),
        word(9, 360, 1800),
        word(0, 0, 400_000_000),
        word(6, 0, 30),
        word(9, 0, 799999),
        word(7, 0, 2),
    ];
    let mut seed = 0x144c_c001_u64;
    for _ in 0..33 {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        words.push(seed);
    }
    for (name, clock) in [
        (
            "soroban",
            ChainClock::Soroban {
                ledger: 100000,
                timestamp: 1701000000,
            },
        ),
        (
            "evm",
            ChainClock::Evm {
                timestamp: 1701000000,
            },
        ),
    ] {
        let mut model = ModelRouter::new(clock);
        let mut out = String::from("# word,success,result,user,treasury,rail,router,recipient0,recipient1,paused,fee,nonce,claims,actions,flow0,flow1,flow2,flow3,liability0,liability1\n");
        for word in &words {
            let result = model.apply(Operation::decode(*word));
            assert!(model.conserved());
            write!(
                &mut out,
                "{},{},{}",
                word,
                u8::from(result.is_ok()),
                result.unwrap_or(0)
            )
            .unwrap();
            for value in model.balances {
                write!(&mut out, ",{value}").unwrap();
            }
            write!(
                &mut out,
                ",{},{},{},{},{}",
                u8::from(model.paused),
                model.fee_bps,
                model.nonce,
                model.claim_count,
                model.action_count
            )
            .unwrap();
            for rail in [
                Rail::Cctp,
                Rail::AxelarIts,
                Rail::AxelarGmp,
                Rail::Allbridge,
            ] {
                write!(&mut out, ",{}", model.available(rail)).unwrap();
            }
            let l = model.liabilities();
            writeln!(&mut out, ",{},{}", l[0], l[1]).unwrap();
        }
        let path = Path::new(&root).join(format!("router-model-{name}.csv"));
        if env::args().any(|arg| arg == "--check") {
            assert_eq!(fs::read_to_string(path).unwrap(), out, "stale model corpus");
        } else {
            fs::write(path, out).unwrap();
        }
    }
}
