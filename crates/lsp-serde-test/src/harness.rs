use rand::rngs::StdRng;
use rand::SeedableRng;
use serde_json::Value;

pub struct TestResult {
    pub type_name: &'static str,
    pub total: usize,
    pub failures: Vec<Failure>,
}

pub struct Failure {
    pub input: Value,
    pub orig: Outcome,
    pub fork: Outcome,
}

#[derive(Debug)]
#[allow(dead_code)] // The Err message is only read through Debug.
pub enum Outcome {
    Ok(Value),
    Err(String),
}

pub trait TypeTest: Send + Sync {
    fn type_name(&self) -> &'static str;
    fn generate(&self, rng: &mut StdRng) -> Vec<Value>;
    fn round_trip_orig(&self, json: &Value) -> Outcome;
    fn round_trip_fork(&self, json: &Value) -> Outcome;

    fn run(&self, count: usize, seed: u64) -> TestResult {
        let mut rng = StdRng::seed_from_u64(seed);
        let mut failures = Vec::new();
        let mut total = 0;

        for _ in 0..count {
            let inputs = self.generate(&mut rng);
            total += inputs.len();
            for input in inputs {
                let orig = self.round_trip_orig(&input);
                let fork = self.round_trip_fork(&input);
                if !outcomes_match(&orig, &fork) {
                    failures.push(Failure { input, orig, fork });
                }
            }
        }

        TestResult {
            type_name: self.type_name(),
            total,
            failures,
        }
    }
}

fn outcomes_match(a: &Outcome, b: &Outcome) -> bool {
    match (a, b) {
        (Outcome::Ok(va), Outcome::Ok(vb)) => va == vb,
        (Outcome::Err(_), Outcome::Err(_)) => true,
        _ => false,
    }
}

pub fn print_results(results: &[TestResult], verbose: bool) {
    let mut any_failed = false;
    for r in results {
        if r.failures.is_empty() {
            println!("  PASS  {} ({} inputs)", r.type_name, r.total);
        } else {
            any_failed = true;
            println!(
                "  FAIL  {} ({}/{} inputs failed)",
                r.type_name,
                r.failures.len(),
                r.total
            );
            if verbose {
                for (i, f) in r.failures.iter().enumerate().take(3) {
                    println!("    failure {}:", i + 1);
                    println!("      input: {}", f.input);
                    println!("      orig:  {:?}", f.orig);
                    println!("      fork:  {:?}", f.fork);
                }
                if r.failures.len() > 3 {
                    println!("    ... and {} more", r.failures.len() - 3);
                }
            }
        }
    }
    if any_failed {
        std::process::exit(1);
    }
}

#[macro_export]
macro_rules! type_test {
    ($struct_name:ident, $type_name:ident, $gen_fn:expr) => {
        $crate::type_test!(
            $struct_name,
            $type_name,
            lsp_types::$type_name,
            lsp_types_manual::$type_name,
            $gen_fn
        );
    };
    ($struct_name:ident, $label:ident, $orig:path, $fork:path, $gen_fn:expr) => {
        pub struct $struct_name;

        impl $crate::harness::TypeTest for $struct_name {
            fn type_name(&self) -> &'static str {
                stringify!($label)
            }

            fn generate(&self, rng: &mut rand::rngs::StdRng) -> Vec<serde_json::Value> {
                $gen_fn(rng)
            }

            fn round_trip_orig(&self, json: &serde_json::Value) -> $crate::harness::Outcome {
                match serde_json::from_value::<$orig>(json.clone()) {
                    Ok(v) => match serde_json::to_value(v) {
                        Ok(out) => $crate::harness::Outcome::Ok(out),
                        Err(e) => $crate::harness::Outcome::Err(e.to_string()),
                    },
                    Err(e) => $crate::harness::Outcome::Err(e.to_string()),
                }
            }

            fn round_trip_fork(&self, json: &serde_json::Value) -> $crate::harness::Outcome {
                match serde_json::from_value::<$fork>(json.clone()) {
                    Ok(v) => match serde_json::to_value(v) {
                        Ok(out) => $crate::harness::Outcome::Ok(out),
                        Err(e) => $crate::harness::Outcome::Err(e.to_string()),
                    },
                    Err(e) => $crate::harness::Outcome::Err(e.to_string()),
                }
            }
        }
    };
}
