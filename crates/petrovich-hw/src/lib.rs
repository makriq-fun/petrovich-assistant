//! Аппаратная индикация и ввод: светодиоды, часы, кнопки, мут.

pub mod buttons;
pub mod clock;
pub mod leds;
pub mod light;

use std::path::PathBuf;
use std::time::Duration;

use chrono::Timelike as _;
use petrovich_core::config::HardwareConfig;
use petrovich_core::events::{AlarmEvent, TimerEvent, UiEvent, UiNotice, VoiceState};
use petrovich_core::profile::HardwareProfile;
use tokio::sync::mpsc;

pub use buttons::ButtonEvent;

pub struct Hardware {
    pub buttons: mpsc::UnboundedReceiver<ButtonEvent>,
}

/// Запускает индикацию и ввод. Ошибки оборудования не фатальны: демон
/// продолжает голосовую работу, а диагностика сообщает причину.
pub fn start(
    profile: &'static HardwareProfile,
    cfg: &HardwareConfig,
    ui: mpsc::UnboundedReceiver<UiEvent>,
) -> Hardware {
    let (btn_tx, btn_rx) = mpsc::unbounded_channel();
    if cfg.buttons {
        let device = cfg
            .input_device
            .clone()
            .unwrap_or_else(|| profile.input_device.to_owned());
        buttons::start_buttons(Some(&device), cfg.action_key, btn_tx.clone());
        buttons::start_mute(cfg.mic_mute_gpio.or(profile.mute_gpio), btn_tx.clone());
    }
    let gpio = cfg.mic_mute_gpio.or(profile.mute_gpio);
    let _ = gpio;
    let leds_on = cfg.leds;
    let clock_on = cfg.clock;
    let profile: &'static HardwareProfile = profile;
    let led_brightness = cfg.led_brightness;
    let clock_brightness = cfg.clock_brightness;
    let auto_brightness = cfg.auto_brightness;
    let light_sensor = cfg.light_sensor.clone();
    let night_brightness = cfg.night_brightness;
    tokio::spawn(async move {
        run_ui(
            profile,
            leds_on,
            clock_on,
            led_brightness,
            clock_brightness,
            auto_brightness,
            light_sensor,
            night_brightness,
            ui,
        )
        .await
    });
    Hardware { buttons: btn_rx }
}

#[allow(clippy::too_many_arguments)]
async fn run_ui(
    profile: &'static HardwareProfile,
    leds_on: bool,
    clock_on: bool,
    led_brightness: f32,
    clock_brightness: f32,
    auto_brightness: bool,
    light_sensor: Option<PathBuf>,
    night_brightness: f32,
    mut ui: mpsc::UnboundedReceiver<UiEvent>,
) {
    let mut leds = if leds_on {
        match leds::Leds::open(
            profile,
            std::path::Path::new(leds::DEFAULT_LED_DIR),
            led_brightness,
        ) {
            Ok(l) => Some(l),
            Err(e) => {
                tracing::warn!("светодиоды: {e:#}");
                None
            }
        }
    } else {
        None
    };
    let mut clock = clock_on.then(|| {
        clock::Clock::open(
            profile,
            std::path::Path::new(clock::DEFAULT_LED_DIR),
            clock_brightness,
        )
    });
    if clock.as_ref().is_some_and(|c| !c.enabled()) {
        tracing::info!("часы не найдены; индикация времени отключена");
    }

    let mut sensor = if auto_brightness {
        light::LightSensor::open(light_sensor.as_deref())
    } else {
        None
    };
    match (&sensor, auto_brightness) {
        (Some(s), _) => tracing::info!("датчик освещённости: {}", s.path().display()),
        (None, true) => {
            tracing::info!("датчик освещённости не найден; яркость фиксированная")
        }
        (None, false) => {}
    }

    let mut state = VoiceState::Idle;
    let mut muted = false;
    let mut volume_percent = 0;
    let mut volume_until = tokio::time::Instant::now();
    let mut error_until = tokio::time::Instant::now();
    let mut notice: Option<(UiNotice, tokio::time::Instant)> = None;
    let mut timer_deadline: Option<tokio::time::Instant> = None;
    let mut alarm_time: Option<(u32, u32)> = None;
    let mut alarm_until = tokio::time::Instant::now();
    let start = tokio::time::Instant::now();
    let mut last = start;
    let mut light_target = 1.0f32;
    let mut light_factor = 1.0f32;
    let mut light_next = start;
    let mut shown = [[0.0f32; 3]; 6];
    let mut interval = tokio::time::interval(Duration::from_millis(20));
    loop {
        tokio::select! {
            _ = interval.tick() => {}
            ev = ui.recv() => {
                match ev {
                    None => return,
                    Some(UiEvent::State(s)) => state = s,
                    Some(UiEvent::Notice(n)) => {
                        notice = Some((n, tokio::time::Instant::now()));
                    }
                    Some(UiEvent::Error(e)) => {
                        tracing::warn!("UI: {e}");
                        error_until = tokio::time::Instant::now() + Duration::from_secs(2);
                    }
                    Some(UiEvent::Volume(v)) => {
                        volume_percent = v;
                        volume_until = tokio::time::Instant::now() + Duration::from_millis(1500);
                    }
                    Some(UiEvent::Timer(TimerEvent::Started { seconds, .. })) => {
                        timer_deadline = Some(
                            tokio::time::Instant::now() + Duration::from_secs(seconds),
                        );
                    }
                    Some(UiEvent::Timer(TimerEvent::Fired { .. })) => {
                        timer_deadline = None;
                        notice = Some((UiNotice::Timer, tokio::time::Instant::now()));
                    }
                    Some(UiEvent::Timer(TimerEvent::Cancelled { .. })) => {
                        timer_deadline = None;
                    }
                    Some(UiEvent::Alarm(AlarmEvent::Started { hour, minute, .. })) => {
                        alarm_time = Some((hour, minute));
                        alarm_until = tokio::time::Instant::now() + Duration::from_secs(3);
                    }
                    Some(UiEvent::Alarm(AlarmEvent::Fired { .. })) => {
                        alarm_time = None;
                        notice = Some((UiNotice::Alarm, tokio::time::Instant::now()));
                    }
                    Some(UiEvent::Emotion(e)) => {
                        notice = Some((UiNotice::Emotion(e), tokio::time::Instant::now()));
                    }
                    Some(UiEvent::Muted(m)) => muted = m,
                    Some(UiEvent::SpeechStarted) => state = VoiceState::Speaking,
                    Some(UiEvent::SpeechFinished) => state = VoiceState::Idle,
                }
                continue;
            }
        }

        let now = tokio::time::Instant::now();
        let dt = now.duration_since(last).as_secs_f32().clamp(0.001, 0.2);
        last = now;
        let t = now.duration_since(start).as_secs_f32();

        if auto_brightness && now >= light_next {
            light_next = now + Duration::from_secs(1);
            if let Some(s) = sensor.as_mut() {
                if let Some(lux) = s.read() {
                    light_target = light::brightness_factor(lux, night_brightness);
                }
            }
        }
        let light_alpha = 1.0 - (-dt / 1.2).exp();
        light_factor += (light_target - light_factor) * light_alpha;

        if let Some((n, at)) = notice {
            if now.duration_since(at) > leds::notice_duration(n) {
                notice = None;
            }
        }

        let base = if muted {
            leds::mute_colors(t)
        } else if now < volume_until {
            leds::volume_colors(volume_percent)
        } else if now < error_until {
            leds::state_colors(VoiceState::Error, t)
        } else {
            leds::state_colors(state, t)
        };
        let mut target = [[0.0f32; 3]; 6];
        for (i, c) in base.iter().enumerate().take(6) {
            for ch in 0..3 {
                target[i][ch] = f32::from(c[ch]);
            }
        }
        if let Some((n, at)) = notice {
            let overlay = leds::notice_colors(n, now.duration_since(at).as_secs_f32());
            for (i, c) in overlay.iter().enumerate().take(6) {
                for ch in 0..3 {
                    target[i][ch] = (target[i][ch] + f32::from(c[ch])).min(255.0);
                }
            }
        }
        let alpha = 1.0 - (-dt / 0.06).exp();
        for i in 0..6 {
            for ch in 0..3 {
                shown[i][ch] += (target[i][ch] - shown[i][ch]) * alpha;
            }
        }
        if let Some(l) = leds.as_mut() {
            l.set_brightness((led_brightness * light_factor).clamp(0.0, 1.0));
            let out: Vec<[u8; 3]> = shown
                .iter()
                .map(|p| {
                    [
                        p[0].round().clamp(0.0, 255.0) as u8,
                        p[1].round().clamp(0.0, 255.0) as u8,
                        p[2].round().clamp(0.0, 255.0) as u8,
                    ]
                })
                .collect();
            if let Err(e) = l.set(&out) {
                tracing::warn!("светодиоды: {e:#}");
                leds = None;
            }
        }

        if let Some(c) = clock.as_mut().filter(|c| c.enabled()) {
            c.set_brightness((clock_brightness * light_factor).clamp(0.0, 1.0));
            let r = if now < volume_until {
                c.show_volume(volume_percent)
            } else if now < alarm_until {
                match alarm_time {
                    Some((hour, minute)) => c.show_time_level(hour, minute, 0),
                    None => clock_time(c, state, t),
                }
            } else if let Some(dl) = timer_deadline {
                if dl > now {
                    c.show_timer(dl.duration_since(now).as_secs())
                } else {
                    timer_deadline = None;
                    c.all_off()
                }
            } else if let Some((n, _at)) = notice {
                let masks = clock::notice_masks(n);
                if masks != [0; 4] {
                    let level = (f32::from(c.level()) * 0.6).round() as u8;
                    c.set_levels(&masks, 0, level)
                } else {
                    clock_time(c, state, t)
                }
            } else if state == VoiceState::Thinking {
                let masks = clock::spinner_masks(t);
                let colon = (f32::from(c.level()) * (0.2 + 0.3 * pulse(t, 0.8))).round() as u8;
                c.set_levels(&masks, colon, c.level())
            } else {
                clock_time(c, state, t)
            };
            if let Err(e) = r {
                tracing::warn!("часы: {e:#}");
                clock = None;
            }
        }
    }
}

fn clock_time(c: &mut clock::Clock, state: VoiceState, t: f32) -> anyhow::Result<()> {
    let now = chrono::Local::now();
    let period = match state {
        VoiceState::Listening => 1.2,
        VoiceState::Error => 0.5,
        _ => 2.0,
    };
    let colon = match state {
        VoiceState::Speaking => 1.0,
        _ => 0.12 + 0.88 * smoothstep(pulse(t, period)),
    };
    let level = (f32::from(c.level()) * colon).round() as u8;
    c.show_time_level(now.hour(), now.minute(), level)
}

fn pulse(t: f32, period: f32) -> f32 {
    0.5 - 0.5 * (2.0 * std::f32::consts::PI * t / period).cos()
}

fn smoothstep(x: f32) -> f32 {
    let x = x.clamp(0.0, 1.0);
    x * x * (3.0 - 2.0 * x)
}
