---
name: burn-framework
description: Use when implementing neural networks, tensor operations, training, or inference with the Rust Burn framework.
---

# Burn Framework

- Consult current official Burn documentation.
- Verify every API against the exact version in Cargo.lock.
- Explain tensor dimensions and device/backend choices.
- Separate model architecture, data pipeline, training, and inference.
- Test forward passes and tensor shapes.
- Verify gradients and optimizer updates.
- Test checkpoint save/load and inference reproducibility.
- Confirm backend compatibility before enabling GPU acceleration.
- Run cargo check, cargo test, and cargo clippy.
- Do not invent Burn APIs or copy examples from incompatible versions.
