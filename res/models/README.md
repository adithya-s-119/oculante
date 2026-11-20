# Embedded photo classifier

The Photos gallery ships a tiny image classifier so categories work fully
offline — no account, no upload, no network call.

## SqueezeNet 1.1

- File: `squeezenet1.1-7.onnx` (4.7 MB)
- Architecture: [SqueezeNet 1.1](https://arxiv.org/abs/1602.07360) (Iandola et al.)
- Source: [ONNX Model Zoo](https://huggingface.co/onnxmodelzoo/squeezenet1.1-7)
- License: Apache-2.0
- Dataset: ImageNet ILSVRC2012 (1000 classes)
- Reported accuracy: 56.34% top-1 / 79.12% top-5

Inference is performed by [tract](https://github.com/sonos/tract), a pure-Rust
ONNX runtime. The model is embedded into the executable with `include_bytes!`.

## Labels

`synset.txt` is the standard ImageNet class list from the ONNX Model Zoo. The
gallery maps those 1000 specific labels down to a handful of friendly
categories (trees, cats, dogs, birds, insects, animals, food, vehicles, places
and objects) in `src/ml.rs`.

## Performance

Only a 224×224 centre crop is fed to the network. A 480×360 gallery thumbnail
is reused, results are cached in
`%LOCALAPPDATA%\pictor\ml_cache.json` (or the platform equivalent) keyed by file
path and modification time, and inference happens on a single background
thread. Browsing and zooming are never blocked by classification.
