//! Multi-horizon neural network that forecasts the price at each horizon together with
//! how far off that forecast is likely to be.
//!
//! - Input projection with SiLU, then a residual block (linear, SiLU); no normalisation
//!   layer, so the function stays smooth everywhere and attributions are exact
//! - Per horizon: a point head (forecast return) and a scale head (log expected price error)
//! - Loss: smooth version of the tiered price credit in [`PRICE_TIERS`], plus a Huber term
//!   that keeps far-off rows informative, plus a Laplace likelihood for the scale heads
//! - AdamW with analytical backpropagation, gradient clipping and multi-threaded batches
//! - Integrated gradients attribute each forecast to the input features

use std::thread;

use serde::{Deserialize, Serialize};

use crate::features::{NUM_FEATURES, Sample};
use crate::metrics::{EvaluationReport, HORIZONS, PRICE_TIERS, compute_metrics, price_error};

const NUM_HORIZONS: usize = HORIZONS.len();

/// Linear layer with weights and biases.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinearLayer {
    pub in_dim: usize,
    pub out_dim: usize,
    /// Weights matrix of shape [out_dim, in_dim] in row-major order.
    pub weights: Vec<f64>,
    pub bias: Vec<f64>,
}

impl LinearLayer {
    pub fn new(in_dim: usize, out_dim: usize, rng_seed: &mut u64) -> Self {
        // Xavier/Glorot uniform initialization
        let limit = (6.0 / (in_dim + out_dim) as f64).sqrt();
        let num_weights = in_dim * out_dim;
        let mut weights = Vec::with_capacity(num_weights);
        for _ in 0..num_weights {
            *rng_seed = rng_seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let u = (*rng_seed >> 32) as f64 / 4294967296.0;
            weights.push((u * 2.0 - 1.0) * limit);
        }
        LinearLayer {
            in_dim,
            out_dim,
            weights,
            bias: vec![0.0; out_dim],
        }
    }

    pub fn forward(&self, input: &[f64]) -> Vec<f64> {
        let mut out = self.bias.clone();
        for (o, slot) in out.iter_mut().enumerate() {
            let row = &self.weights[o * self.in_dim..(o + 1) * self.in_dim];
            *slot += row.iter().zip(input).map(|(w, x)| w * x).sum::<f64>();
        }
        out
    }
}

/// Layer Normalization layer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LayerNorm {
    pub dim: usize,
    pub gamma: Vec<f64>,
    pub beta: Vec<f64>,
}

impl LayerNorm {
    pub fn new(dim: usize) -> Self {
        LayerNorm {
            dim,
            gamma: vec![1.0; dim],
            beta: vec![0.0; dim],
        }
    }

    pub fn forward(&self, x: &[f64]) -> (Vec<f64>, f64, f64) {
        let n = self.dim as f64;
        let mean = x.iter().sum::<f64>() / n;
        let var = x.iter().map(|&v| (v - mean).powi(2)).sum::<f64>() / n;
        let inv_std = 1.0 / (var + 1e-5).sqrt();
        let out = (0..self.dim)
            .map(|i| self.gamma[i] * (x[i] - mean) * inv_std + self.beta[i])
            .collect();
        (out, mean, inv_std)
    }

    // Gradient w.r.t. the layer input, given the gradient w.r.t. its output `out`.
    pub fn backward(&self, grad_out: &[f64], out: &[f64], inv_std: f64) -> Vec<f64> {
        let n = self.dim as f64;
        let x_hat: Vec<f64> = (0..self.dim)
            .map(|i| (out[i] - self.beta[i]) / self.gamma[i])
            .collect();
        let g: Vec<f64> = (0..self.dim).map(|i| grad_out[i] * self.gamma[i]).collect();
        let mean_g = g.iter().sum::<f64>() / n;
        let mean_gx = g.iter().zip(&x_hat).map(|(a, b)| a * b).sum::<f64>() / n;
        (0..self.dim)
            .map(|i| inv_std * (g[i] - mean_g - x_hat[i] * mean_gx))
            .collect()
    }
}

/// Weights of the training objective.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LossConfig {
    // Width of the sigmoid ramps that replace the hard tier edges, in price-error units.
    pub temperature: f64,
    // Weight of the Huber term relative to the tiered credit.
    pub huber_weight: f64,
    pub huber_delta: f64,
    // Weight of the Laplace likelihood that trains the scale heads.
    pub scale_weight: f64,
}

impl Default for LossConfig {
    fn default() -> Self {
        LossConfig {
            temperature: 0.004,
            huber_weight: 0.2,
            huber_delta: 0.02,
            scale_weight: 0.1,
        }
    }
}

// Credit gained when crossing into each band, tightest band first; sums to the top credit.
fn tier_increments() -> [(f64, f64); PRICE_TIERS.len()] {
    let mut out = [(0.0, 0.0); PRICE_TIERS.len()];
    for (i, (band, credit)) in PRICE_TIERS.iter().enumerate() {
        let next = PRICE_TIERS.get(i + 1).map_or(0.0, |(_, c)| *c);
        out[i] = (*band, credit - next);
    }
    out
}

#[inline]
fn sigmoid(x: f64) -> f64 {
    1.0 / (1.0 + (-x).exp())
}

// Smooth tiered credit of an absolute price error and its derivative w.r.t. that error.
fn soft_tier(error: f64, temperature: f64) -> (f64, f64) {
    let mut score = 0.0;
    let mut slope = 0.0;
    for (band, inc) in tier_increments() {
        let s = sigmoid((band - error) / temperature);
        score += inc * s;
        slope -= inc * s * (1.0 - s) / temperature;
    }
    (score, slope)
}

/// Expected tiered credit when the price error is Laplace-distributed with mean
/// absolute value `scale`: P(|e| <= band) = 1 - exp(-band / scale).
pub fn expected_tier_score(scale: f64) -> f64 {
    let scale = scale.max(1e-6);
    tier_increments()
        .iter()
        .map(|(band, inc)| inc * (1.0 - (-band / scale).exp()))
        .sum()
}

/// Multi-Horizon Neural Network model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MultiHorizonNet {
    pub in_dim: usize,
    pub hidden_dim: usize,
    pub fc_in: LinearLayer,
    pub fc_hidden: LinearLayer,
    /// Forecast return per horizon in [`HORIZONS`].
    pub heads: Vec<LinearLayer>,
    /// Log of the expected absolute price error per horizon.
    #[serde(default)]
    pub scale_heads: Vec<LinearLayer>,
}

/// One forecast for every horizon.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Forecast {
    pub returns: [f64; NUM_HORIZONS],
    /// Expected absolute price error (Laplace scale).
    pub error_scale: [f64; NUM_HORIZONS],
}

impl Forecast {
    pub fn expected_score(&self, h_idx: usize) -> f64 {
        expected_tier_score(self.error_scale[h_idx])
    }
}

// Intermediate values of one forward pass, kept for backpropagation.
struct Trace {
    z1: Vec<f64>,
    a1: Vec<f64>,
    z2: Vec<f64>,
    a2: Vec<f64>,
    points: [f64; NUM_HORIZONS],
    log_scales: [f64; NUM_HORIZONS],
}

impl MultiHorizonNet {
    pub fn new(hidden_dim: usize, mut seed: u64) -> Self {
        let fc_in = LinearLayer::new(NUM_FEATURES, hidden_dim, &mut seed);
        let fc_hidden = LinearLayer::new(hidden_dim, hidden_dim, &mut seed);
        // Point heads start close to the no-change forecast; the tiered credit has no
        // gradient for errors far beyond the widest band.
        let heads = (0..NUM_HORIZONS)
            .map(|_| {
                let mut layer = LinearLayer::new(hidden_dim, 1, &mut seed);
                layer.weights.iter_mut().for_each(|w| *w *= 0.01);
                layer
            })
            .collect();
        // Scale heads start near the typical error, which grows with the square root of time.
        let scale_heads = HORIZONS
            .iter()
            .map(|&h| {
                let mut layer = LinearLayer::new(hidden_dim, 1, &mut seed);
                layer.weights.iter_mut().for_each(|w| *w *= 0.1);
                layer.bias[0] = (0.015 * (h as f64).sqrt()).ln();
                layer
            })
            .collect();

        MultiHorizonNet {
            in_dim: NUM_FEATURES,
            hidden_dim,
            fc_in,
            fc_hidden,
            heads,
            scale_heads,
        }
    }

    // a1 = silu(fc_in(x)); a2 = a1 + silu(fc_hidden(a1)). Inputs are already standardised
    // and clipped, and without a normalisation layer the output keeps responding to
    // feature magnitudes (LayerNorm made x and 2x look alike and is singular near zero).
    fn trace(&self, input: &[f64]) -> Trace {
        let z1 = self.fc_in.forward(input);
        let a1: Vec<f64> = z1.iter().map(|&v| silu(v)).collect();
        let z2 = self.fc_hidden.forward(&a1);
        let a2: Vec<f64> = a1.iter().zip(&z2).map(|(r, &z)| r + silu(z)).collect();
        let mut points = [0.0; NUM_HORIZONS];
        let mut log_scales = [0.0; NUM_HORIZONS];
        for h in 0..NUM_HORIZONS {
            points[h] = self.heads[h].forward(&a2)[0];
            log_scales[h] = self
                .scale_heads
                .get(h)
                .map_or(0.02f64.ln(), |head| head.forward(&a2)[0]);
        }
        Trace {
            z1,
            a1,
            z2,
            a2,
            points,
            log_scales,
        }
    }

    /// Forward pass returning predicted returns for all horizons.
    pub fn forward(&self, input: &[f64]) -> Vec<f64> {
        self.trace(input).points.to_vec()
    }

    pub fn forecast(&self, input: &[f64]) -> Forecast {
        let t = self.trace(input);
        Forecast {
            returns: t.points,
            error_scale: t.log_scales.map(|z| z.clamp(-12.0, 3.0).exp()),
        }
    }

    #[allow(clippy::too_many_arguments)]
    // Backpropagates output gradients through the network. Adds `scale` times the
    // parameter gradient into `grad` (laid out like `get_params`) when given, and returns
    // the gradient w.r.t. the input when `want_input` is set.
    fn backward(
        &self,
        input: &[f64],
        t: &Trace,
        d_points: &[f64; NUM_HORIZONS],
        d_log_scales: &[f64; NUM_HORIZONS],
        scale: f64,
        mut grad: Option<&mut [f64]>,
        want_input: bool,
    ) -> Option<Vec<f64>> {
        let h = self.hidden_dim;
        let fc_in_b = h * NUM_FEATURES;
        let fc_hidden_w = fc_in_b + h;
        let fc_hidden_b = fc_hidden_w + h * h;
        let heads = fc_hidden_b + h;
        let scale_heads = heads + NUM_HORIZONS * (h + 1);

        let mut grad_a2 = vec![0.0; h];
        for h_idx in 0..NUM_HORIZONS {
            let scale_head = self
                .scale_heads
                .get(h_idx)
                .map(|l| (l, d_log_scales[h_idx], scale_heads));
            for (layer, d, base) in
                std::iter::once((&self.heads[h_idx], d_points[h_idx], heads)).chain(scale_head)
            {
                if d == 0.0 {
                    continue;
                }
                let offset = base + h_idx * (h + 1);
                if let Some(g) = grad.as_deref_mut() {
                    for (gw, a) in g[offset..offset + h].iter_mut().zip(&t.a2) {
                        *gw += scale * d * a;
                    }
                    g[offset + h] += scale * d;
                }
                for (ga, w) in grad_a2.iter_mut().zip(&layer.weights) {
                    *ga += d * w;
                }
            }
        }

        // Residual block: the skip path carries grad_a2 straight to a1.
        let grad_z2: Vec<f64> = (0..h).map(|i| grad_a2[i] * silu_grad(t.z2[i])).collect();
        let mut grad_n = vec![0.0; h];
        for (o, &d) in grad_z2.iter().enumerate() {
            let row = o * h;
            if let Some(g) = grad.as_deref_mut() {
                g[fc_hidden_b + o] += scale * d;
                for (gw, n) in g[fc_hidden_w + row..fc_hidden_w + row + h]
                    .iter_mut()
                    .zip(&t.a1)
                {
                    *gw += scale * d * n;
                }
            }
            for (gn, w) in grad_n.iter_mut().zip(&self.fc_hidden.weights[row..row + h]) {
                *gn += d * w;
            }
        }
        let grad_z1: Vec<f64> = (0..h)
            .map(|i| (grad_a2[i] + grad_n[i]) * silu_grad(t.z1[i]))
            .collect();
        if let Some(g) = grad {
            for (o, &dz) in grad_z1.iter().enumerate() {
                let d = scale * dz;
                g[fc_in_b + o] += d;
                let row = o * NUM_FEATURES;
                for (gw, x) in g[row..row + NUM_FEATURES].iter_mut().zip(input) {
                    *gw += d * x;
                }
            }
        }
        want_input.then(|| {
            let mut gx = vec![0.0; NUM_FEATURES];
            for (o, &d) in grad_z1.iter().enumerate() {
                let row = &self.fc_in.weights[o * NUM_FEATURES..(o + 1) * NUM_FEATURES];
                for (g, w) in gx.iter_mut().zip(row) {
                    *g += d * w;
                }
            }
            gx
        })
    }

    /// Integrated gradients of the forecast return at `h_idx` from the all-zero row
    /// (the training mean after scaling) to `input`. Returns per-feature attributions,
    /// the forecast at the baseline and at `input`; attributions sum to their difference.
    pub fn integrated_gradients(
        &self,
        input: &[f64],
        h_idx: usize,
        steps: usize,
    ) -> (Vec<f64>, f64, f64) {
        let steps = steps.max(1);
        let mut d_points = [0.0; NUM_HORIZONS];
        d_points[h_idx] = 1.0;
        let zeros = [0.0; NUM_HORIZONS];
        let mut avg = vec![0.0; NUM_FEATURES];
        for k in 0..steps {
            let alpha = (k as f64 + 0.5) / steps as f64;
            let x: Vec<f64> = input.iter().map(|v| v * alpha).collect();
            let t = self.trace(&x);
            if let Some(g) = self.backward(&x, &t, &d_points, &zeros, 0.0, None, true) {
                for (a, gi) in avg.iter_mut().zip(g) {
                    *a += gi / steps as f64;
                }
            }
        }
        let attributions = avg.iter().zip(input).map(|(g, x)| g * x).collect();
        let base = self.trace(&vec![0.0; NUM_FEATURES]).points[h_idx];
        let pred = self.trace(input).points[h_idx];
        (attributions, base, pred)
    }

    /// Forecasts for every sample, computed on up to `threads` threads.
    pub fn forecast_all(&self, samples: &[Sample], threads: usize) -> Vec<Forecast> {
        let threads = threads.max(1);
        let chunk = samples.len().div_ceil(threads).max(1);
        thread::scope(|scope| {
            let handles: Vec<_> = samples
                .chunks(chunk)
                .map(|part| {
                    scope.spawn(move || {
                        part.iter()
                            .map(|s| self.forecast(&s.features))
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            handles
                .into_iter()
                .flat_map(|h| h.join().unwrap_or_default())
                .collect()
        })
    }

    pub fn evaluate(&self, samples: &[Sample], split_name: &str) -> EvaluationReport {
        let forecasts = self.forecast_all(samples, default_threads());
        evaluate_forecasts(&forecasts, samples, split_name)
    }
}

pub fn evaluate_forecasts(
    forecasts: &[Forecast],
    samples: &[Sample],
    split_name: &str,
) -> EvaluationReport {
    let horizons = HORIZONS
        .iter()
        .enumerate()
        .map(|(h_idx, &h)| {
            let actual: Vec<f64> = samples.iter().map(|s| s.forward_returns[h_idx]).collect();
            let pred: Vec<f64> = forecasts.iter().map(|f| f.returns[h_idx]).collect();
            compute_metrics(h, &actual, &pred)
        })
        .collect();
    EvaluationReport {
        model_name: "Multi-Horizon Deep Neural Network".to_owned(),
        split_name: split_name.to_owned(),
        horizons,
    }
}

pub fn default_threads() -> usize {
    thread::available_parallelism().map_or(4, |n| n.get().min(16))
}

#[inline]
fn silu(x: f64) -> f64 {
    x / (1.0 + (-x).exp())
}

#[inline]
fn silu_grad(x: f64) -> f64 {
    let sig = 1.0 / (1.0 + (-x).exp());
    sig * (1.0 + x * (1.0 - sig))
}

#[inline]
fn huber_loss(pred: f64, actual: f64, delta: f64) -> f64 {
    let diff = (pred - actual).abs();
    if diff <= delta {
        0.5 * diff * diff
    } else {
        delta * (diff - 0.5 * delta)
    }
}

#[inline]
fn huber_grad(pred: f64, actual: f64, delta: f64) -> f64 {
    let diff = pred - actual;
    if diff.abs() <= delta {
        diff
    } else {
        delta * diff.signum()
    }
}

/// AdamW Optimizer state.
#[derive(Debug)]
pub struct AdamW {
    pub lr: f64,
    pub beta1: f64,
    pub beta2: f64,
    pub eps: f64,
    pub weight_decay: f64,
    pub step: u64,
    pub m: Vec<f64>,
    pub v: Vec<f64>,
}

impl AdamW {
    pub fn new(num_params: usize, lr: f64) -> Self {
        AdamW {
            lr,
            beta1: 0.9,
            beta2: 0.999,
            eps: 1e-8,
            weight_decay: 0.01,
            step: 0,
            m: vec![0.0; num_params],
            v: vec![0.0; num_params],
        }
    }

    pub fn step(&mut self, params: &mut [f64], grads: &[f64]) {
        self.step += 1;
        let beta1 = self.beta1;
        let beta2 = self.beta2;
        let beta1_t = beta1.powi(self.step as i32);
        let beta2_t = beta2.powi(self.step as i32);

        for i in 0..params.len() {
            let g = grads[i];
            self.m[i] = beta1 * self.m[i] + (1.0 - beta1) * g;
            self.v[i] = beta2 * self.v[i] + (1.0 - beta2) * g * g;

            let m_hat = self.m[i] / (1.0 - beta1_t);
            let v_hat = self.v[i] / (1.0 - beta2_t);

            // AdamW decoupled weight decay
            params[i] -= self.lr * self.weight_decay * params[i];
            params[i] -= (self.lr * m_hat) / (v_hat.sqrt() + self.eps);
        }
    }
}

/// Flatten all trainable parameters into a single vector.
pub fn get_params(net: &MultiHorizonNet) -> Vec<f64> {
    let mut p = Vec::new();
    p.extend_from_slice(&net.fc_in.weights);
    p.extend_from_slice(&net.fc_in.bias);
    p.extend_from_slice(&net.fc_hidden.weights);
    p.extend_from_slice(&net.fc_hidden.bias);
    for head in net.heads.iter().chain(&net.scale_heads) {
        p.extend_from_slice(&head.weights);
        p.extend_from_slice(&head.bias);
    }
    p
}

/// Set all trainable parameters from a single vector.
pub fn set_params(net: &mut MultiHorizonNet, p: &[f64]) {
    let mut offset = 0;
    let mut copy = |dest: &mut [f64]| {
        let len = dest.len();
        dest.copy_from_slice(&p[offset..offset + len]);
        offset += len;
    };
    copy(&mut net.fc_in.weights);
    copy(&mut net.fc_in.bias);
    copy(&mut net.fc_hidden.weights);
    copy(&mut net.fc_hidden.bias);
    for head in net.heads.iter_mut().chain(net.scale_heads.iter_mut()) {
        copy(&mut head.weights);
        copy(&mut head.bias);
    }
}

// Fisher-Yates shuffle driven by SplitMix64; `state` carries over between epochs.
pub fn shuffle_indices(order: &mut [usize], state: &mut u64) {
    for i in (1..order.len()).rev() {
        *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = *state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        // Multiply-shift maps the 64-bit draw onto 0..=i without modulo bias worth caring about.
        let j = ((u128::from(z) * (i as u128 + 1)) >> 64) as usize;
        order.swap(i, j);
    }
}

// Loss of one sample summed over horizons, with its derivatives w.r.t. the outputs.
fn sample_loss(
    t: &Trace,
    actual: &[f64],
    cfg: &LossConfig,
) -> (f64, [f64; NUM_HORIZONS], [f64; NUM_HORIZONS]) {
    let mut loss = 0.0;
    let mut d_points = [0.0; NUM_HORIZONS];
    let mut d_scales = [0.0; NUM_HORIZONS];
    for h in 0..NUM_HORIZONS {
        let (p, a) = (t.points[h], actual[h]);
        let gross = (1.0 + a).max(1e-6);
        let signed = (1.0 + p) / gross - 1.0;
        let error = signed.abs();
        let (credit, slope) = soft_tier(error, cfg.temperature);
        let delta = cfg.huber_delta;
        loss += -credit + cfg.huber_weight * huber_loss(p, a, delta) / delta;
        d_points[h] =
            -slope * signed.signum() / gross + cfg.huber_weight * huber_grad(p, a, delta) / delta;

        // Laplace negative log-likelihood of the price error. It also pulls the forecast
        // towards the realised price, harder where the model claims to be confident.
        if cfg.scale_weight > 0.0 {
            let raw = t.log_scales[h];
            let z = raw.clamp(-12.0, 3.0);
            let inv_b = (-z).exp();
            loss += cfg.scale_weight * (error * inv_b + z);
            if raw == z {
                d_scales[h] = cfg.scale_weight * (1.0 - error * inv_b);
            }
            d_points[h] += cfg.scale_weight * inv_b * signed.signum() / gross;
        }
    }
    (loss, d_points, d_scales)
}

// Adds `scale` times the gradient of one sample's loss into `grad` and returns that loss.
fn accumulate_gradient(
    net: &MultiHorizonNet,
    s: &Sample,
    cfg: &LossConfig,
    scale: f64,
    grad: &mut [f64],
) -> f64 {
    let t = net.trace(&s.features);
    let (loss, d_points, d_scales) = sample_loss(&t, &s.forward_returns, cfg);
    net.backward(
        &s.features,
        &t,
        &d_points,
        &d_scales,
        scale,
        Some(grad),
        false,
    );
    loss
}

/// Mean training loss of `samples` under `cfg`, for monitoring.
pub fn mean_loss(net: &MultiHorizonNet, samples: &[Sample], cfg: &LossConfig) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    samples
        .iter()
        .map(|s| sample_loss(&net.trace(&s.features), &s.forward_returns, cfg).0)
        .sum::<f64>()
        / samples.len() as f64
}

/// Visits `train_samples` in `order`, one optimizer step per batch, splitting each batch
/// across `threads` threads; `on_batch(done, total)` runs after every step.
/// Returns the mean loss.
#[allow(clippy::too_many_arguments)]
pub fn train_epoch_with_progress(
    net: &mut MultiHorizonNet,
    optimizer: &mut AdamW,
    train_samples: &[Sample],
    order: &[usize],
    batch_size: usize,
    cfg: &LossConfig,
    threads: usize,
    on_batch: &mut dyn FnMut(u64, u64),
) -> f64 {
    let n = order.len();
    if n == 0 {
        return 0.0;
    }
    let threads = threads.max(1);
    let batch_size = batch_size.max(1);
    let mut param_vec = get_params(net);
    let mut partials = vec![vec![0.0; param_vec.len()]; threads];
    let mut grad_sum = vec![0.0; param_vec.len()];
    let batches = n.div_ceil(batch_size) as u64;
    let mut total_loss = 0.0;

    for (batch_idx, chunk) in order.chunks(batch_size).enumerate() {
        let scale = 1.0 / chunk.len() as f64;
        let per_thread = chunk.len().div_ceil(threads).max(1);
        let shared: &MultiHorizonNet = net;
        total_loss += thread::scope(|scope| {
            let handles: Vec<_> = chunk
                .chunks(per_thread)
                .zip(partials.iter_mut())
                .map(|(part, grad)| {
                    scope.spawn(move || {
                        grad.fill(0.0);
                        part.iter()
                            .map(|&i| {
                                accumulate_gradient(shared, &train_samples[i], cfg, scale, grad)
                            })
                            .sum::<f64>()
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().unwrap_or(0.0))
                .sum::<f64>()
        });
        let used = chunk.len().div_ceil(per_thread);
        grad_sum.fill(0.0);
        for part in &partials[..used] {
            for (g, p) in grad_sum.iter_mut().zip(part) {
                *g += p;
            }
        }

        // Clip gradients to norm 1.0 for training stability
        let norm = grad_sum.iter().map(|&g| g * g).sum::<f64>().sqrt();
        if norm > 1.0 {
            for g in &mut grad_sum {
                *g /= norm;
            }
        }

        optimizer.step(&mut param_vec, &grad_sum);
        set_params(net, &param_vec);
        on_batch(batch_idx as u64 + 1, batches);
    }

    total_loss / n as f64
}

/// Price error of a forecast return against the realised return.
pub fn forecast_error(forecast: &Forecast, h_idx: usize, actual_return: f64) -> f64 {
    price_error(forecast.returns[h_idx], actual_return)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draw(state: &mut u64) -> f64 {
        *state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        (*state >> 32) as f64 / 4294967296.0 - 0.5
    }

    fn sample(state: &mut u64) -> Sample {
        Sample {
            symbol: "005930".to_owned(),
            date: chrono::NaiveDate::from_ymd_opt(2024, 1, 2).expect("valid date"),
            features: (0..NUM_FEATURES).map(|_| draw(state) * 4.0).collect(),
            // Errors land inside the tier ramps, in the Huber linear zone, and far out.
            forward_returns: vec![0.005, -0.012, 0.3, -0.4, 0.035],
            target_end: chrono::NaiveDate::from_ymd_opt(2024, 2, 1).expect("valid date"),
            trading_value: 0.0,
        }
    }

    // Central differences on the full loss must match backpropagation.
    #[test]
    fn analytic_gradient_matches_finite_differences() {
        let mut net = MultiHorizonNet::new(6, 7);
        let mut state = 11u64;
        let s = sample(&mut state);
        let cfg = LossConfig {
            temperature: 0.01,
            ..LossConfig::default()
        };
        let loss_at = |net: &MultiHorizonNet| -> f64 {
            sample_loss(&net.trace(&s.features), &s.forward_returns, &cfg).0
        };

        let params = get_params(&net);
        let mut grad = vec![0.0; params.len()];
        let loss = accumulate_gradient(&net, &s, &cfg, 1.0, &mut grad);
        assert!((loss - loss_at(&net)).abs() < 1e-12);

        let eps = 1e-6;
        for i in 0..params.len() {
            let mut shifted = params.clone();
            shifted[i] = params[i] + eps;
            set_params(&mut net, &shifted);
            let up = loss_at(&net);
            shifted[i] = params[i] - eps;
            set_params(&mut net, &shifted);
            let down = loss_at(&net);
            let numeric = (up - down) / (2.0 * eps);
            assert!(
                (numeric - grad[i]).abs() < 1e-5 * (1.0 + numeric.abs()),
                "param {i}: numeric {numeric} analytic {}",
                grad[i]
            );
        }
    }

    #[test]
    fn input_gradient_matches_finite_differences_and_ig_is_complete() {
        let net = MultiHorizonNet::new(8, 3);
        let mut state = 5u64;
        let s = sample(&mut state);
        let mut d = [0.0; NUM_HORIZONS];
        d[2] = 1.0;
        let t = net.trace(&s.features);
        let gx = net
            .backward(&s.features, &t, &d, &[0.0; NUM_HORIZONS], 0.0, None, true)
            .expect("input grad");
        let eps = 1e-6;
        for i in 0..NUM_FEATURES {
            let mut x = s.features.clone();
            x[i] += eps;
            let up = net.forward(&x)[2];
            x[i] -= 2.0 * eps;
            let down = net.forward(&x)[2];
            assert!(((up - down) / (2.0 * eps) - gx[i]).abs() < 1e-6);
        }
        let (attr, base, pred) = net.integrated_gradients(&s.features, 2, 64);
        let total: f64 = attr.iter().sum();
        assert!(
            (total - (pred - base)).abs() < 1e-3 * (1.0 + (pred - base).abs()),
            "sum {total} vs {}",
            pred - base
        );
    }

    #[test]
    fn soft_tier_approaches_the_step_credit() {
        for e in [0.0, 0.005, 0.015, 0.025, 0.035, 0.045, 0.08] {
            let (soft, _) = soft_tier(e, 1e-4);
            assert!((soft - crate::metrics::tier_score(e)).abs() < 1e-6, "{e}");
        }
        assert!(expected_tier_score(1e-4) > 0.999);
        assert!(expected_tier_score(1.0) < 0.05);
    }

    #[test]
    fn threaded_epoch_matches_single_thread() {
        let mut state = 9u64;
        let samples: Vec<Sample> = (0..50).map(|_| sample(&mut state)).collect();
        let order: Vec<usize> = (0..samples.len()).collect();
        let cfg = LossConfig::default();
        let run = |threads| {
            let mut net = MultiHorizonNet::new(8, 1);
            let mut opt = AdamW::new(get_params(&net).len(), 1e-3);
            train_epoch_with_progress(
                &mut net,
                &mut opt,
                &samples,
                &order,
                16,
                &cfg,
                threads,
                &mut |_, _| {},
            );
            get_params(&net)
        };
        let (a, b) = (run(1), run(4));
        assert!(a.iter().zip(&b).all(|(x, y)| (x - y).abs() < 1e-9));
    }

    #[test]
    fn shuffle_is_a_seeded_permutation() {
        let mut a: Vec<usize> = (0..1000).collect();
        let mut b = a.clone();
        let (mut sa, mut sb) = (42, 42);
        shuffle_indices(&mut a, &mut sa);
        shuffle_indices(&mut b, &mut sb);
        assert_eq!(a, b, "same seed, same order");
        assert_ne!(a, (0..1000).collect::<Vec<_>>());

        let first = a.clone();
        shuffle_indices(&mut a, &mut sa);
        assert_ne!(a, first, "state advances between epochs");
        a.sort_unstable();
        assert_eq!(a, (0..1000).collect::<Vec<_>>());
    }
}
