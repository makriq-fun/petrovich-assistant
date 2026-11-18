// C API вокруг TFLM microfrontend: 16 кГц PCM -> int8 features.
#include <cstdint>
#include <cstdlib>
#include <cstring>
#include "tensorflow/lite/experimental/microfrontend/lib/frontend.h"
#include "tensorflow/lite/experimental/microfrontend/lib/frontend_util.h"

extern "C" {
struct mf_handle {
  FrontendConfig cfg;
  FrontendState st;
};

mf_handle *mf_create(void) {
  auto *h = static_cast<mf_handle *>(std::calloc(1, sizeof(mf_handle)));
  if (!h) return nullptr;
  h->cfg.window.size_ms = 30;
  h->cfg.window.step_size_ms = 10;
  h->cfg.filterbank.num_channels = 40;
  h->cfg.filterbank.lower_band_limit = 125.0f;
  h->cfg.filterbank.upper_band_limit = 7500.0f;
  h->cfg.noise_reduction.smoothing_bits = 10;
  h->cfg.noise_reduction.even_smoothing = 0.025f;
  h->cfg.noise_reduction.odd_smoothing = 0.06f;
  h->cfg.noise_reduction.min_signal_remaining = 0.05f;
  h->cfg.pcan_gain_control.enable_pcan = 1;
  h->cfg.pcan_gain_control.strength = 0.95f;
  h->cfg.pcan_gain_control.offset = 80.0f;
  h->cfg.pcan_gain_control.gain_bits = 21;
  h->cfg.log_scale.enable_log = 1;
  h->cfg.log_scale.scale_shift = 6;
  if (!FrontendPopulateState(&h->cfg, &h->st, 16000)) {
    std::free(h);
    return nullptr;
  }
  return h;
}

// Возвращает 1, если сформирован кадр из 40 int8; 0 — мало данных.
int mf_process(mf_handle *h, const int16_t *samples, size_t count,
               int8_t out[40]) {
  size_t read = 0;
  FrontendOutput o = FrontendProcessSamples(&h->st, samples, count, &read);
  if (o.size != 40) return 0;
  const int32_t scale = 256;
  const int32_t div = 666;
  for (size_t i = 0; i < 40; ++i) {
    int32_t v = ((static_cast<int32_t>(o.values[i]) * scale) + div / 2) / div;
    v += INT8_MIN;
    if (v < INT8_MIN) v = INT8_MIN;
    if (v > INT8_MAX) v = INT8_MAX;
    out[i] = static_cast<int8_t>(v);
  }
  return 1;
}

void mf_reset(mf_handle *h) {
  FrontendFreeStateContents(&h->st);
  FrontendPopulateState(&h->cfg, &h->st, 16000);
}

void mf_free(mf_handle *h) {
  if (!h) return;
  FrontendFreeStateContents(&h->st);
  std::free(h);
}
}
