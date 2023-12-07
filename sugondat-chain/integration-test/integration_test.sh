#!/bin/bash

cargo build -p sugondat-node --release

subxt metadata --url "ws://127.0.0.1:9988" --file "sugondat-metadata.scale"

../../zombienet.sh

cargo run -p integration-test
