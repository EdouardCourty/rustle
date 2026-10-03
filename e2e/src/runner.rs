//! Scenario registry and parallel runner: bounded concurrency, timeout per
//! scenario, panics caught, report grouped by suite (first path segment).

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::harness::{proxy::OUTPUTS, Env};

pub type Fut = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>>;
type RunFn = Arc<dyn Fn(Arc<Env>) -> Fut + Send + Sync>;

pub struct Scenario {
    pub name: String,
    pub quick: bool,
    pub timeout: Duration,
    run: RunFn,
}

impl Scenario {
    /// Included in `--quick` (the subset `make check` runs).
    pub fn quick(mut self) -> Self {
        self.quick = true;
        self
    }

    pub fn timeout(mut self, t: Duration) -> Self {
        self.timeout = t;
        self
    }
}

pub fn scenario<F, R>(name: impl Into<String>, f: F) -> Scenario
where
    F: Fn(Arc<Env>) -> R + Send + Sync + 'static,
    R: Future<Output = anyhow::Result<()>> + Send + 'static,
{
    Scenario {
        name: name.into(),
        quick: false,
        timeout: Duration::from_secs(60),
        run: Arc::new(move |env| Box::pin(f(env))),
    }
}

/// Returned by a scenario that cannot run here (missing kernel feature,
/// old curl): reported as SKIP with the reason, not as a failure.
#[derive(Debug)]
pub struct Skip(pub String);

impl std::fmt::Display for Skip {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for Skip {}

pub fn skip(reason: impl Into<String>) -> anyhow::Error {
    Skip(reason.into()).into()
}

enum Outcome {
    Ok,
    Skip(String),
    Fail(String),
}

struct Done {
    name: String,
    outcome: Outcome,
    took: Duration,
}

fn tail(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

pub async fn run(env: Arc<Env>, scenarios: Vec<Scenario>, jobs: usize) -> bool {
    let total = scenarios.len();
    let sem = Arc::new(tokio::sync::Semaphore::new(jobs.max(1)));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Done>();
    let t0 = Instant::now();
    for s in scenarios {
        let (env, sem, tx) = (env.clone(), sem.clone(), tx.clone());
        tokio::spawn(async move {
            let _permit = sem.acquire_owned().await.unwrap();
            let outputs = Arc::new(Mutex::new(Vec::new()));
            let start = Instant::now();
            let fut = OUTPUTS.scope(outputs.clone(), (s.run)(env));
            let res = tokio::spawn(tokio::time::timeout(s.timeout, fut)).await;
            let outcome = match res {
                Ok(Ok(Ok(()))) => Outcome::Ok,
                Ok(Ok(Err(e))) => match e.downcast_ref::<Skip>() {
                    Some(sk) => Outcome::Skip(sk.0.clone()),
                    None => Outcome::Fail(format!("{e:#}")),
                },
                Ok(Err(_)) => Outcome::Fail(format!("timeout after {:?}", s.timeout)),
                Err(e) => Outcome::Fail(format!("panic: {e}")),
            };
            let outcome = match outcome {
                Outcome::Fail(mut msg) => {
                    for (dir, out) in outputs.lock().unwrap().iter() {
                        let out = out.lock().unwrap();
                        msg.push_str(&format!("\n    --- proxy {dir} (stderr, tail) ---\n"));
                        for l in tail(&out, 15).lines() {
                            msg.push_str(&format!("    {l}\n"));
                        }
                    }
                    Outcome::Fail(msg)
                }
                o => o,
            };
            let _ = tx.send(Done {
                name: s.name,
                outcome,
                took: start.elapsed(),
            });
        });
    }
    drop(tx);

    let mut done: Vec<Done> = Vec::with_capacity(total);
    while let Some(d) = rx.recv().await {
        let (tag, extra) = match &d.outcome {
            Outcome::Ok => ("\x1b[32m  OK\x1b[0m", String::new()),
            Outcome::Skip(r) => ("\x1b[33mSKIP\x1b[0m", format!("  ({r})")),
            Outcome::Fail(_) => ("\x1b[31mFAIL\x1b[0m", String::new()),
        };
        println!(
            "[{:>4}/{total}] {tag} {:<58} {:>6.2}s{extra}",
            done.len() + 1,
            d.name,
            d.took.as_secs_f64()
        );
        done.push(d);
    }

    // Report by suite.
    done.sort_by(|a, b| a.name.cmp(&b.name));
    let mut suites: Vec<(String, [usize; 3])> = Vec::new();
    for d in &done {
        let suite = d.name.split('/').next().unwrap_or("").to_string();
        if suites.last().map(|s| &s.0) != Some(&suite) {
            suites.push((suite, [0; 3]));
        }
        let c = &mut suites.last_mut().unwrap().1;
        match d.outcome {
            Outcome::Ok => c[0] += 1,
            Outcome::Skip(_) => c[1] += 1,
            Outcome::Fail(_) => c[2] += 1,
        }
    }
    println!("\n{:<14} {:>5} {:>5} {:>5}", "suite", "ok", "skip", "fail");
    for (s, c) in &suites {
        println!("{s:<14} {:>5} {:>5} {:>5}", c[0], c[1], c[2]);
    }
    let fails: Vec<&Done> = done
        .iter()
        .filter(|d| matches!(d.outcome, Outcome::Fail(_)))
        .collect();
    let skips = done
        .iter()
        .filter(|d| matches!(d.outcome, Outcome::Skip(_)))
        .count();
    for d in &fails {
        if let Outcome::Fail(msg) = &d.outcome {
            println!("\n\x1b[31mFAIL\x1b[0m {}\n  {msg}", d.name);
        }
    }
    println!(
        "\n{} scenarios: {} ok, {skips} skipped, {} failed — {:.1}s",
        total,
        total - skips - fails.len(),
        fails.len(),
        t0.elapsed().as_secs_f64()
    );
    fails.is_empty()
}
