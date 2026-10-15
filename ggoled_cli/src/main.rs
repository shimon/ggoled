use clap::{Parser, ValueEnum};
use core::str;
use ggoled_draw::DrawDevice;
use ggoled_draw::bitmap_from_memory;
use ggoled_draw::decode_frames;
use ggoled_lib::Bitmap;
use ggoled_lib::Device;
use spin_sleep::sleep;
use std::sync::Arc;
use std::time::Instant;
mod watch;

use std::{
    io::{Read, stdin},
    ops::Div,
    str::FromStr,
    time::Duration,
};

#[derive(Clone, Copy)]
enum DrawPos {
    Coord(isize),
    Center,
}
impl FromStr for DrawPos {
    type Err = &'static str;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.parse::<isize>().map(DrawPos::Coord).or_else(|_| {
            if ["center", "c"].contains(&s.to_lowercase().as_str()) {
                Ok(DrawPos::Center)
            } else {
                Err("not a valid position")
            }
        })
    }
}
impl DrawPos {
    fn to_option(self) -> Option<isize> {
        match self {
            DrawPos::Coord(c) => Some(c),
            DrawPos::Center => None,
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum Alignment {
    #[value(alias("l"))]
    Left,
    #[value(alias("c"))]
    Center,
    #[value(alias("r"))]
    Right,
}

#[derive(Clone, Copy, ValueEnum)]
enum ScrollSpeed {
    Slow,
    Normal,
    Fast,
}

#[derive(clap::Args)]
struct DrawArgs {
    #[arg(
        short = 'x',
        long,
        help = "Screen X offset for draw commands",
        default_value = "center"
    )]
    screen_x: DrawPos,

    #[arg(
        short = 'y',
        long,
        help = "Screen Y offset for draw commands",
        default_value = "center"
    )]
    screen_y: DrawPos,
}

#[derive(clap::Args)]
struct ImageArgs {
    #[command(flatten)]
    draw_args: DrawArgs,

    #[arg(
        short = 'T',
        long,
        help = "Grayscale threshold for converting images to 1-bit",
        default_value = "100"
    )]
    threshold: u8,

    #[arg(short = 'C', long, help = "Clear the screen before drawing")]
    clear: bool,
}

#[derive(Parser)]
enum Args {
    #[command(about = "Clear the entire screen to black")]
    Clear,

    #[command(about = "Fill the entire screen to white")]
    Fill,

    #[command(about = "Return to SteelSeries UI")]
    Return,

    #[command(about = "Draw some text")]
    Text {
        #[command(flatten)]
        draw_args: DrawArgs,

        #[arg(help = "Text, or omitted for stdin", index = 1)]
        text: Option<String>,

        #[arg(short = 'd', long, help = "Screen delimiter line for stdin input")]
        delimiter: Option<String>,
    },

    #[command(about = "Draw an image")]
    Img {
        #[command(flatten)]
        image_args: ImageArgs,

        #[arg(help = "Image path, or - for stdin", index = 1)]
        path: String,
    },

    #[command(about = "Draw a sequence of images")]
    Anim {
        #[command(flatten)]
        image_args: ImageArgs,

        #[arg(
            short = 'r',
            long,
            help = "Frames to show per second (fps) - defaults to 1 fps or embedded delays for gif files"
        )]
        framerate: Option<u32>,

        #[arg(
            short = 'l',
            long,
            help = "Amount of repetitions, or 0 for infinite",
            default_value = "1"
        )]
        loops: usize,

        #[arg(help = "Image paths", index = 1)]
        paths: Vec<String>,
    },

    #[command(about = "Set display brightness")]
    Brightness {
        #[arg(help = "Brightness, 1-10", index = 1)]
        value: u8,
    },

    #[command(
        about = "Watch headset connection state, running hooks on changes",
        long_about = "Watch headset connection state, running hooks on changes.

Runs until killed, printing the state as a JSON line on stdout whenever it changes. \
Can run alongside ggoled_app (tested on Linux).",
        after_long_help = r#"Output:
  One JSON object per line, e.g.:
    {"time":1792016123,"base_station":true,"wireless":true,"bluetooth":false,"bluetooth_on":true,"battery":6,"charging":1}

  time          Unix timestamp (seconds) of the report
  base_station  Whether the base station is connected over USB
  wireless      Whether the headset is connected to the base station
  bluetooth     Whether a Bluetooth device is connected
  bluetooth_on  Whether Bluetooth is enabled
  battery       Headset battery level, 0-8 (raw device value)
  charging      Charging state (raw device value; meaning not fully known)

  Fields that haven't been reported by the device yet are null.

Hooks:
  Hooks run via `sh -c` (`cmd /C` on Windows) only when `wireless` changes, and
  once on startup for the initial state unless --no-initial-hook is given.
  Battery changes produce output but don't run hooks. If the base station is
  unplugged, the headset is treated as disconnected.

  Hooks receive the new state in environment variables:
    GGOLED_BASE_STATION, GGOLED_WIRELESS, GGOLED_BLUETOOTH  "true" / "false"
    GGOLED_BATTERY, GGOLED_CHARGING                         number, or empty if unknown

  The state file is not removed when ggoled exits.

Example (Linux, switch default audio output):
  ggoled watch \
    --on-connect 'pactl set-default-sink alsa_output.usb-SteelSeries_Arctis_Nova_Pro_Wireless-00.analog-stereo' \
    --on-disconnect 'pactl set-default-sink <your speakers sink>'

  List sink names with `pactl list short sinks`."#
    )]
    Watch(watch::WatchArgs),

    #[command(about = "Print version")]
    Version,

    #[command(about = "Dump devices list to stdout", hide = true)]
    DumpDevices,

    #[command(about = "Probe device for info", hide = true)]
    Probe,
}

fn main() {
    let args = Args::parse();
    if let Args::Version = args {
        println!("{}", env!("CARGO_PKG_VERSION"));
        return;
    } else if let Args::DumpDevices = args {
        Device::dump_devices();
        return;
    } else if let Args::Watch(watch_args) = args {
        watch::watch(watch_args);
        return;
    }

    let dev = Device::connect().unwrap();
    match args {
        Args::Clear => dev.draw(&Bitmap::new(dev.width, dev.height, false), 0, 0).unwrap(),
        Args::Fill => dev.draw(&Bitmap::new(dev.width, dev.height, true), 0, 0).unwrap(),
        Args::Return => dev.return_to_ui().unwrap(),
        Args::Text {
            text,
            draw_args,
            delimiter,
        } => {
            let mut dev = DrawDevice::new(dev, 30);
            // TODO: oneshot text should not try scrolling
            if let Some(text) = text {
                dev.add_text(&text, draw_args.screen_x.to_option(), draw_args.screen_y.to_option());
                dev.play();
            } else {
                dev.play();
                // iterate each line in stdin and draw to screen either when reaching EOF or when encountering `delimiter`
                let mut lines = vec![];
                for line in stdin().lines() {
                    let line = line.expect("Failed to read from stdin").replace('\r', "");
                    if Some(&line) == delimiter.as_ref() {
                        dev.clear_layers();
                        dev.add_text(
                            &lines.join("\n"),
                            draw_args.screen_x.to_option(),
                            draw_args.screen_y.to_option(),
                        );
                        lines.clear();
                    } else {
                        lines.push(line);
                    }
                }
                if !lines.is_empty() {
                    dev.clear_layers();
                    dev.add_text(
                        &lines.join("\n"),
                        draw_args.screen_x.to_option(),
                        draw_args.screen_y.to_option(),
                    );
                }
            }
        }
        Args::Img { path, image_args } => {
            let bitmap = if path == "-" {
                let mut buf = Vec::<u8>::new();
                stdin().read_to_end(&mut buf).expect("Failed to read from stdin");
                Arc::new(bitmap_from_memory(&buf, image_args.threshold).expect("Failed to read image from stdin"))
            } else {
                let mut frames = decode_frames(&path, image_args.threshold);
                if frames.is_empty() {
                    eprintln!("No frames in image");
                    std::process::exit(1);
                } else if frames.len() != 1 {
                    eprintln!("img only supports images with single frame");
                }
                frames.swap_remove(0).bitmap
            };
            let cx = (dev.width as isize - bitmap.w as isize) / 2;
            let cy = (dev.height as isize - bitmap.h as isize) / 2;
            let x = image_args.draw_args.screen_x.to_option().unwrap_or(cx);
            let y = image_args.draw_args.screen_y.to_option().unwrap_or(cy);
            if image_args.clear {
                let mut screen = Bitmap::new(dev.width, dev.height, false);
                screen.blit(&bitmap, x, y, true);
                dev.draw(&screen, 0, 0).unwrap();
            } else {
                dev.draw(&bitmap, x, y).unwrap();
            }
        }
        Args::Anim {
            framerate,
            loops,
            paths,
            image_args,
        } => {
            if framerate == Some(0) {
                eprintln!("Framerate must be non-zero");
                std::process::exit(1);
            } else if paths.is_empty() {
                eprintln!("No image paths");
                std::process::exit(1);
            }
            let period = framerate.map(|f| Duration::from_secs(1).div(f));
            let bitmaps: Vec<(Arc<Bitmap>, Duration)> = paths
                .iter()
                .flat_map(|path| {
                    decode_frames(path, image_args.threshold).into_iter().map(|frame| {
                        (
                            frame.bitmap,
                            period.unwrap_or(frame.delay.unwrap_or(Duration::from_secs(1))),
                        )
                    })
                })
                .collect();
            let draw_animation = || {
                for (bitmap, delay) in &bitmaps {
                    let now_time = Instant::now();
                    let next_frame = now_time + *delay;
                    let cx = (dev.width as isize - bitmap.w as isize) / 2;
                    let cy = (dev.height as isize - bitmap.h as isize) / 2;
                    let x = image_args.draw_args.screen_x.to_option().unwrap_or(cx);
                    let y = image_args.draw_args.screen_y.to_option().unwrap_or(cy);
                    if image_args.clear {
                        let mut screen = Bitmap::new(dev.width, dev.height, false);
                        screen.blit(bitmap, x, y, true);
                        dev.draw(&screen, 0, 0).unwrap();
                    } else {
                        dev.draw(bitmap, x, y).unwrap();
                    }
                    if now_time < next_frame {
                        sleep(next_frame.duration_since(Instant::now()));
                    } else {
                        println!("fell behind - framerate too fast");
                    }
                }
            };
            if loops == 0 {
                loop {
                    draw_animation();
                }
            } else {
                for _ in 0..loops {
                    draw_animation();
                }
            }
        }
        Args::Brightness { value } => {
            dev.set_brightness(value).unwrap();
        }
        // handled above
        Args::Version | Args::DumpDevices | Args::Watch(_) => {}
        Args::Probe => {
            dev.probe().unwrap();
            loop {
                for event in dev.poll_event().unwrap() {
                    println!("Event: {event:?}");
                }
            }
        }
    }
}
