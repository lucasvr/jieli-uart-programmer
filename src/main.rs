use boot::{BootParams, LOADER, Loader, UBOOT};
use clap::{Args, CommandFactory, Parser, Subcommand};
use console::Console;
use dongle::{Dongle, Driver};
use eyre::{Result, ensure, eyre};
use flash::{Flash, Progress};
use jieli_image::{FlashImage, Packer, Uboot};
use link::Link;
use power::Psu;
use std::path::PathBuf;
use std::time::Duration;

mod boot;
mod console;
mod dongle;
mod find;
mod flash;
mod jieli_image;
mod link;
mod power;

/// Flashes JieLi SH55 chips (AD15N, AD152A4) through their UART boot ROM.
///
/// Requires the jieli-programmer dongle (ESP32-C6) wired to PB9, a DPS150
/// power supply.
#[derive(Parser, Debug)]
#[command(version, about, long_about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Prints flash and chip key information.
    Info(Hardware),
    /// Reads the flash into a file.
    Read(Read),
    /// Packs an app and writes it to flash.
    Write(Write),
    /// Power-cycles the chip and prints its console output.
    Console(ConsoleArgs),
    /// Writes an app to flash and prints its console output.
    Run(Run),
    /// Packs an app into a flash image without using the hardware.
    Pack(Pack),
    /// Checks the CRCs of a flash image.
    Check(Check),
    /// Controls the power supply.
    Power(PowerArgs),
}

#[derive(Args, Debug)]
struct Hardware {
    /// Dongle serial port [default: found by USB id].
    #[arg(long, env = "JIELI_DONGLE")]
    dongle: Option<String>,

    /// DPS150 serial port [default: found by USB id].
    #[arg(long, env = "JIELI_PSU")]
    psu: Option<String>,

    /// Seconds to keep the supply off before booting.
    #[arg(long, default_value_t = 8)]
    off_secs: u64,

    /// Boot attempts before giving up.
    #[arg(long, default_value_t = 3)]
    attempts: usize,

    /// Prints the bytes exchanged with the loader.
    #[arg(short, long)]
    verbose: bool,
}

#[derive(Args, Debug)]
struct Read {
    /// Output file.
    output: PathBuf,

    #[arg(long, default_value = "0", value_parser = parse_num)]
    addr: u32,

    /// Bytes to read [default: the whole flash].
    #[arg(long, value_parser = parse_num)]
    len: Option<u32>,

    #[command(flatten)]
    hw: Hardware,
}

#[derive(Args, Debug)]
struct Write {
    /// Flat app binary linked at 0x100100.
    image: PathBuf,

    /// Writes IMAGE unmodified at --addr instead of packing it.
    #[arg(long)]
    raw: bool,

    /// Address for --raw.
    #[arg(long, default_value = "0", value_parser = parse_num, requires = "raw")]
    addr: u32,

    #[command(flatten)]
    hw: Hardware,
}

#[derive(Args, Debug)]
struct ConsoleArgs {
    #[arg(long, default_value_t = 9600)]
    baud: u32,

    /// Seconds to run [default: until interrupted].
    #[arg(long)]
    secs: Option<f64>,

    #[command(flatten)]
    hw: Hardware,
}

#[derive(Args, Debug)]
struct Run {
    /// Flat app binary linked at 0x100100.
    image: PathBuf,

    /// Console baud rate.
    #[arg(long, default_value_t = 9600)]
    baud: u32,

    #[command(flatten)]
    hw: Hardware,
}

#[derive(Args, Debug)]
struct Pack {
    /// Flat app binary linked at 0x100100.
    app: PathBuf,

    /// Flash key, as printed by `info`.
    #[arg(long, value_parser = parse_num)]
    key: u32,

    #[arg(short, long)]
    output: PathBuf,

    #[arg(long, default_value = "0x80000", value_parser = parse_num)]
    flash_size: u32,
}

#[derive(Args, Debug)]
struct Check {
    image: PathBuf,
}

#[derive(Args, Debug)]
struct PowerArgs {
    state: PowerState,

    /// DPS150 serial port [default: found by USB id].
    #[arg(long, env = "JIELI_PSU")]
    psu: Option<String>,

    /// Seconds to stay off when cycling.
    #[arg(long, default_value_t = 6)]
    off_secs: u64,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
enum PowerState {
    On,
    Off,
    Cycle,
}

fn parse_num(s: &str) -> Result<u32, String> {
    let r = match s.strip_prefix("0x") {
        Some(hex) => u32::from_str_radix(hex, 16),
        None => s.parse(),
    };
    r.map_err(|e| e.to_string())
}

/// The hardware setup: dongle and power supply.
struct Board {
    dongle: String,
    psu: Psu,
    boot: BootParams,
    off: Duration,
    attempts: usize,
    verbose: bool,
}

impl Board {
    fn new(hw: Hardware) -> Result<Self> {
        Ok(Self {
            dongle: Dongle::USB.find(hw.dongle)?,
            psu: Psu::new(Psu::USB.find(hw.psu)?)?,
            boot: BootParams::default(),
            off: Duration::from_secs(hw.off_secs),
            attempts: hw.attempts.max(1),
            verbose: hw.verbose,
        })
    }

    fn open_dongle(&self) -> Result<Dongle> {
        Dongle::open(&self.dongle, self.verbose)
    }

    /// Power-cycles the chip into the loader and opens the flash.
    fn connect(&self) -> Result<Flash> {
        let loader = Loader::parse(LOADER)?;
        eprintln!("loader {} ({} bytes)", loader.version, loader.image.len());
        let header = loader.header(&self.boot);
        let mut d = self.open_dongle()?;
        d.set_baud(self.boot.rom_baud)?;
        for attempt in 1..=self.attempts {
            eprintln!("attempt {attempt}: power-cycling the chip");
            let cycle = self.psu.cycle_later(self.off);
            let r = d.boot(&self.boot, &header, &loader.image)?;
            cycle.join().map_err(|_| eyre!("power thread panicked"))??;
            match (r.status, r.stage) {
                (0, 2) => {
                    eprintln!("loader running at {} baud", r.loader_baud);
                    d.set_driver(Driver::Uart)?;
                    let flash = Flash::open(Link::new(d, Duration::from_secs(3)))?;
                    return Ok(flash);
                }
                (1, _) => eprintln!("attempt {attempt}: the supply never went off"),
                (2, _) => eprintln!("attempt {attempt}: the supply never came back"),
                (_, 1) => {
                    eprintln!("attempt {attempt}: the ROM took the key, the loader didn't answer")
                }
                _ => eprintln!("attempt {attempt}: no answer from the ROM"),
            }
        }
        Err(eyre!(
            "the loader did not start after {} attempts",
            self.attempts
        ))
    }

    /// Writes a packed app, or a raw image at `addr`.
    fn write(&self, image: &PathBuf, raw: Option<u32>) -> Result<Flash> {
        let data = fs_err::read(image)?;
        let mut flash = self.connect()?;
        // stdout is reserved for console output.
        eprintln!("{}", flash.info);
        let (addr, data) = match raw {
            Some(addr) => (addr, data),
            None => {
                let packer = Packer {
                    key: flash.info.flash_key(),
                    flash_size: flash.info.flash_size.unwrap_or(0x8_0000),
                    uboot: Uboot::parse(UBOOT)?,
                };
                (0, packer.pack(&data)?.0)
            }
        };
        flash.write(addr, &data, &Progress::new("write"))?;
        eprintln!("wrote {} bytes at {addr:#x}", data.len());
        Ok(flash)
    }
}

fn main() -> Result<()> {
    // Exit quietly when stdout is a closed pipe.
    #[cfg(unix)]
    // SAFETY: Setting a signal to a constant disposition touches no Rust state.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL)
    };
    clap_complete::CompleteEnv::with_factory(Cli::command)
        .completer("jieli-uart")
        .complete();
    let cli = Cli::parse();
    color_eyre::install()?;
    match cli.command {
        Command::Info(hw) => println!("{}", Board::new(hw)?.connect()?.info),
        Command::Read(args) => {
            let mut flash = Board::new(args.hw)?.connect()?;
            eprintln!("{}", flash.info);
            let len = args.len.or(flash.info.flash_size);
            let len = len.ok_or_else(|| eyre!("flash size unknown; pass --len"))?;
            let data = flash.read(args.addr, len, &Progress::new("read"))?;
            fs_err::write(&args.output, data)?;
        }
        Command::Write(args) => {
            let raw = args.raw.then_some(args.addr);
            Board::new(args.hw)?.write(&args.image, raw)?;
        }
        Command::Console(args) => {
            let board = Board::new(args.hw)?;
            Console::new(board.open_dongle()?).run(&board.psu, args.baud, args.secs)?;
        }
        Command::Run(args) => {
            let board = Board::new(args.hw)?;
            let dongle = board.write(&args.image, None)?.into_dongle();
            Console::new(dongle).run(&board.psu, args.baud, None)?;
        }
        Command::Pack(args) => {
            let packer = Packer {
                key: u16::try_from(args.key)?,
                flash_size: args.flash_size,
                uboot: Uboot::parse(UBOOT)?,
            };
            let image = packer.pack(&fs_err::read(&args.app)?)?;
            fs_err::write(&args.output, image.bytes())?;
            println!(
                "wrote {} ({:#x} bytes)",
                args.output.display(),
                image.bytes().len()
            );
        }
        Command::Check(args) => {
            let report = FlashImage(fs_err::read(&args.image)?).check()?;
            for line in &report.lines {
                println!("{line}");
            }
            ensure!(
                report.mismatches == 0,
                "{} CRC mismatches",
                report.mismatches
            );
            println!("all CRCs ok");
        }
        Command::Power(args) => {
            let psu = Psu::new(Psu::USB.find(args.psu)?)?;
            match args.state {
                PowerState::On => psu.set(true)?,
                PowerState::Off => psu.set(false)?,
                PowerState::Cycle => psu.cycle(Duration::from_secs(args.off_secs))?,
            }
        }
    }
    Ok(())
}
