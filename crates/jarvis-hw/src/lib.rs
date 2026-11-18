//! Аппаратная индикация и ввод: светодиоды, часы, кнопки, мут.

pub mod buttons;
pub mod clock;
pub mod leds;

use std::time::Duration;

use jarvis_core::config::HardwareConfig;
use jarvis_core::events::{TimerEvent, UiEvent, VoiceState};
use jarvis_core::profile::HardwareProfile;
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
    tokio::spawn(async move {
        run_ui(
            profile,
            leds_on,
            clock_on,
            led_brightness,
            clock_brightness,
            ui,
        )
        .await;
    });
    Hardware { buttons: btn_rx }
}

async fn run_ui(
    profile: &'static HardwareProfile,
    leds_on: bool,
    clock_on: bool,
    led_brightness: f32,
    clock_brightness: f32,
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
    let mut state = VoiceState::Idle;
    let mut muted = false;
    let mut volume_percent = 0;
    let mut volume_until = tokio::time::Instant::now();
    let mut error_until = tokio::time::Instant::now();
    let mut timer_deadline: Option<tokio::time::Instant> = None;
    let mut tick: u64 = 0;
    let mut interval = tokio::time::interval(Duration::from_millis(33));
    loop {
        tokio::select! {
            _ = interval.tick() => {}
            ev = ui.recv() => {
                match ev {
                    None => return,
                    Some(UiEvent::State(s)) => state = s,
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
                    Some(UiEvent::Timer(TimerEvent::Fired { .. }))
                    | Some(UiEvent::Timer(TimerEvent::Cancelled { .. })) => {
                        timer_deadline = None;
                    }
                    Some(UiEvent::Muted(m)) => muted = m,
                    Some(UiEvent::SpeechStarted) => state = VoiceState::Speaking,
                    Some(UiEvent::SpeechFinished) => state = VoiceState::Idle,
                }
                continue;
            }
        }
        tick = tick.wrapping_add(1);
        let now = tokio::time::Instant::now();
        if let Some(l) = leds.as_mut() {
            let colors = if muted {
                leds::mute_colors()
            } else if now < volume_until {
                leds::volume_colors(volume_percent)
            } else if now < error_until {
                leds::state_colors(VoiceState::Error, tick)
            } else {
                leds::state_colors(state, tick)
            };
            if let Err(e) = l.set(&colors) {
                tracing::warn!("светодиоды: {e:#}");
                leds = None;
            }
        }
        if tick % 5 != 0 {
            continue;
        }
        if let Some(c) = clock.as_mut().filter(|c| c.enabled()) {
            let r = match timer_deadline {
                Some(dl) if dl > now => {
                    let secs = dl.duration_since(now).as_secs();
                    c.show_timer(secs)
                }
                Some(_) => {
                    timer_deadline = None;
                    c.all_off()
                }
                None => {
                    use chrono::Timelike as _;
                    let t = chrono::Local::now();
                    let colon = t.second() % 2 == 0;
                    c.show_time(t.hour(), t.minute(), colon)
                }
            };
            if let Err(e) = r {
                tracing::warn!("часы: {e:#}");
                clock = None;
            }
        }
    }
}
