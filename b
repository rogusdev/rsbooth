cargo build --release --no-default-features --features pi
#scp rsbooth.toml pibooth@10.0.10.177:~/  # config file
scp target/release/rsbooth pibooth@10.0.10.177:~/
