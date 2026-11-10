---
name: time-series-forecasting
description: Use when designing, implementing, training, or evaluating multi-horizon financial time-series forecasting models.
---

# Time Series Forecasting

- Treat the task as financial multi-horizon forecasting.
- Preserve chronological ordering.
- Never use future information in historical features.
- Support configurable lookback windows and forecast horizons.
- Compare LSTM, GRU, TCN, and Transformer only when justified.
- Evaluate every forecast horizon independently.
- Use walk-forward validation.
- Analyze concept drift and market regime changes.
- Compare against naive and statistical baselines.
- Never claim 80% accuracy without untouched out-of-sample evidence.
