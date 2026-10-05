cargo build --release --no-default-features --features pi
scp target/release/rsbooth rsbooth.toml pi@raspberrypi.local:~/
