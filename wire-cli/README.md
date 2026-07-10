# wire-cli

A command-line interface to make calls with `wire`.

## Usage

```
cargo run -p wire-cli --release -- <node-id>
```

On Windows, or if the build fails, you can disable the audio processing entirely. You should only use Wire with headphones then.
```
cargo run -p wire-cli --release --no-default-features -- <node-id>
```