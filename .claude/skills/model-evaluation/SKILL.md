---
name: model-evaluation
description: Use when validating forecasting models, calculating metrics, or performing financial backtests.
---

# Model Evaluation

- Split training, validation, and test periods chronologically.
- Never tune hyperparameters on the final test period.
- Perform walk-forward validation.
- Report MAE, RMSE, and suitable percentage error metrics.
- Report directional accuracy, precision, recall, F1, and balanced accuracy.
- Evaluate every forecast horizon independently.
- Compare with naive baselines such as previous close and historical drift.
- Report sample counts and uncertainty intervals where appropriate.
- Include transaction costs and slippage in strategy backtests.
- Disclose weak, unstable, or statistically insignificant results.
