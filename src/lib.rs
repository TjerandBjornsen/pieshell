use std::env;
use std::fs;
use std::io::{self, BufReader, BufWriter, Read, Stdin, Stdout, Write};
use std::ops::BitAnd;
use std::path::{Path, PathBuf};
use std::process::{self, Command};
use std::str;
use std::time::Duration;

use rppal::uart::{self, Parity, Uart};

const SHELL_NAME: &str = "pieshell";

enum Reader {
    STDIN(BufReader<Stdin>),
    UART(Uart),
}

enum Writer {
    STDOUT(BufWriter<Stdout>),
    UART(Uart),
}

impl Write for Writer {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Writer::STDOUT(stdout) => stdout.get_mut().write(buf),
            Writer::UART(uart) => match uart.write(buf) {
                Ok(bytes_written) => Ok(bytes_written),
                Err(uart::Error::Io(error)) => Err(error),
                Err(uart::Error::InvalidValue) => Err(io::Error::from(io::ErrorKind::InvalidData)),
                Err(uart::Error::Gpio(error)) => {
                    Err(io::Error::new(io::ErrorKind::Other, error.to_string()))
                }
            },
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Writer::STDOUT(stdout) => stdout.get_mut().flush(),
            Writer::UART(uart) => match uart.flush(uart::Queue::Output) {
                Ok(_) => Ok(()),
                Err(uart::Error::Io(error)) => Err(error),
                Err(uart::Error::InvalidValue) => Err(io::Error::from(io::ErrorKind::InvalidData)),
                Err(uart::Error::Gpio(error)) => {
                    Err(io::Error::new(io::ErrorKind::Other, error.to_string()))
                }
            },
        }
    }
}

impl Writer {
    fn write_ln(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let Ok(mut output) = String::from_utf8(buf.to_vec()) {
            output.push_str("\n");
            self.write(output.as_bytes())
        } else {
            Err(io::Error::new(
                io::ErrorKind::Other,
                "Invalid UTF-8 sequence",
            ))
        }
    }
}

impl Read for Reader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Reader::STDIN(stdin) => stdin.read(buf),
            Reader::UART(uart) => match uart.read(buf) {
                Ok(bytes_read) => Ok(bytes_read),
                Err(uart::Error::Io(error)) => Err(error),
                Err(uart::Error::InvalidValue) => Err(io::Error::from(io::ErrorKind::InvalidData)),
                Err(uart::Error::Gpio(error)) => {
                    Err(io::Error::new(io::ErrorKind::Other, error.to_string()))
                }
            },
        }
    }
}

impl Reader {
    fn read_utf8_char(&mut self) -> io::Result<Option<char>> {
        let mut read_buf = [0u8; 1];
        let mut char_buf = [0u8; 4];

        /* Read first byte */
        if self.read(&mut read_buf[..]).unwrap() == 0 {
            /* "End of file" reached */
            return Ok(None);
        }

        /* Find number of bytes of the UTF-8 character */
        let first_byte = read_buf[0];
        let bytes_in_char = if first_byte.bitand(0x80) == 0x00 {
            1
        } else if first_byte.bitand(0xE0) == 0xC0 {
            2
        } else if first_byte.bitand(0xF0) == 0xE0 {
            3
        } else if first_byte.bitand(0xF8) == 0xF0 {
            4
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{:#x} is not the start of a valid UTF-8 character!",
                    first_byte
                ),
            ));
        };

        /* Read the remaining bytes */
        char_buf[0] = first_byte;
        for i in 1..bytes_in_char {
            if self.read(&mut read_buf[..]).unwrap() == 0 {
                /* Nothing to read, but not end of valid UTF-8 character */
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "{:x?} is not a valid UTF-8 character!",
                        &char_buf[0..(i - 1)]
                    ),
                ));
            }
            char_buf[i] = read_buf[0];
        }

        /* Convert to char */
        match String::from_utf8(char_buf.to_vec()) {
            Ok(c) => Ok(Some(c.chars().next().unwrap())),
            Err(error) => Err(io::Error::new(io::ErrorKind::Other, error)),
        }
    }
}

pub fn run() {
    let (mut reader, mut writer) = create_reader_writer();

    /* Fetch environment variables that will be used in the prompt */
    let user = match env::var("USER") {
        Ok(user) => user,
        Err(_) => String::new(),
    };
    let host_name = match fs::read_to_string("/proc/sys/kernel/hostname") {
        Ok(name) => name.trim().to_owned(),
        Err(_) => String::new(),
    };
    let home = match env::var("HOME") {
        Ok(home) => home,
        Err(_) => String::new(),
    };

    writer.write_ln(b"Welcome to the shell").unwrap();
    loop {
        /* Print prompt */
        let prompt = get_prompt(&user, &host_name, &home);
        writer.write(prompt.as_bytes()).unwrap();
        io::stdout()
            .flush()
            .expect("should be able to flush stdout");

        /* Get input */
        let input = match read_input(&mut reader, &mut writer) {
            Ok(input) => input,
            Err(error) => {
                writer
                    .write_ln(format!("Error while getting input: {:#?}", error).as_bytes())
                    .unwrap();
                process::exit(1);
            }
        };

        /* Parse input */
        let mut command = match parse_input(&input) {
            Ok(Some(command)) => command,
            Ok(None) => continue,
            Err(parse_error) => match parse_error.kind() {
                io::ErrorKind::InvalidInput => {
                    writer
                        .write_ln(
                            format!("{}: {}: No such file or directory", SHELL_NAME, parse_error)
                                .as_bytes(),
                        )
                        .expect("should be able to write error");
                    continue;
                }
                io::ErrorKind::NotFound => {
                    writer
                        .write_ln(format!("{}: command not found", parse_error).as_bytes())
                        .expect("should be able to write error");
                    continue;
                }
                error_kind => {
                    writer
                        .write_ln(
                            format!("Encountered IO error while parsing: {}", error_kind)
                                .as_bytes(),
                        )
                        .expect("should be able to write error");
                    continue;
                }
            },
        };

        /* Execute command */
        match command.output() {
            Ok(output) => {
                let output_string = String::from_utf8(output.stdout).unwrap();
                writer.write(output_string.as_bytes()).unwrap();
            }
            Err(execution_error) => {
                let cmd = command
                    .get_program()
                    .to_str()
                    .expect("parsed command should have a program");
                writer
                    .write_ln(format!("{}: {}: {}", SHELL_NAME, cmd, execution_error).as_bytes())
                    .unwrap();
            }
        }
    }
}

fn create_reader_writer() -> (Reader, Writer) {
    if cfg!(target_arch = "aarch64") {
        let uart_write =
            Uart::new(115_200, Parity::None, 8, 1).expect("Should be able to configure uart");

        /* Read must be last, as set_read_mode() is overwritten by calling
        Uart::new again. */
        let mut uart_read =
            Uart::new(115_200, Parity::None, 8, 1).expect("Should be able to configure uart");
        uart_read
            .set_read_mode(1, Duration::new(0, 0))
            .expect("Should be able to set read mode");

        (Reader::UART(uart_read), Writer::UART(uart_write))
    } else {
        (
            Reader::STDIN(BufReader::new(io::stdin())),
            Writer::STDOUT(BufWriter::new(io::stdout())),
        )
    }
}

fn get_prompt(user: &str, host_name: &str, home: &str) -> String {
    let current_dir = env::current_dir().expect("should be able to get current directory");
    let current_dir_str = current_dir
        .to_str()
        .expect("current dir should be valid UTF-8")
        .replace(home, "~");

    format! {"{}@{}:{}$ ", user, host_name, current_dir_str}
}

fn read_input(reader: &mut Reader, writer: &mut Writer) -> io::Result<String> {
    let mut input = String::new();

    /* Read until a newline or a control character */
    loop {
        let c = match reader.read_utf8_char() {
            Ok(Some(c)) => String::from(c),
            Ok(None) => {
                println!("Exiting program");
                process::exit(1);
            }
            Err(error) => return Err(error),
        };

        /* Echo back character to the UART to give feedback of what was actually
        written. Without this you can't see what you type in the serial
        terminal */
        if cfg!(target_arch = "aarch64") {
            let c = match c.chars().next() {
                Some('\u{3}') => String::from("^C\r"),
                Some('\u{4}') => String::from("exit\r\r"),
                _ => c.clone(),
            };

            writer
                .write(&c.as_bytes())
                .expect("Should be able to write valid UTF-8");
        }

        /* Handle control characters */
        match c.chars().next() {
            Some('\n') |
            /* Check for carriage return as that is what
            is sent by PuTTY when pressing enter */
            Some('\r') => break,
            /* CTRL + C */
            Some('\u{3}') => {
                input.clear();
                break;
            },
            /* CTRL + D */
            Some('\u{4}') => {
                return Ok(c);
            }
            /* Backspace */
            Some('\u{7f}') => {
                input.pop();
                continue;
            }
            _ => {}
        }

        input.push_str(&c);
    }

    Ok(input)
}

fn parse_input(input: &String) -> io::Result<Option<Command>> {
    let args: Vec<&str> = input.trim().split(" ").collect();

    if args[0] == "" {
        return Ok(None);
    }

    /* Check for control characters */
    match args[0].chars().next().unwrap() {
        '\u{4}' => process::exit(1),
        _ => {}
    }
    // TODO: check if command is shell function. Not implemented yet as there
    // are no shell functions to handle yet.

    /* Find the location of the binary */
    match find_binary(args[0]) {
        Ok(Some(full_path)) => {
            let mut command = Command::new(full_path);
            for i in 1..args.len() {
                command.arg(args[i]);
            }
            Ok(Some(command))
        }
        Ok(None) => Err(io::Error::new(io::ErrorKind::NotFound, args[0])),
        Err(error) => Err(error),
    }
}

fn find_binary(program: &str) -> io::Result<Option<PathBuf>> {
    let path = PathBuf::from(program);

    /* Checks if file exist in relative or absolute path */
    if path.parent() != Some(Path::new("")) {
        if path.is_file() {
            return Ok(Some(path));
        } else {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, program));
        }
    }

    /* Fetch the PATH variable */
    let path_variable = match env::var("PATH") {
        Ok(path) => path,
        Err(_error) => return Err(io::Error::new(io::ErrorKind::Other, "failed to fetch PATH")),
    };

    /* Search every directory in PATH for the requested binary */
    for dir in path_variable.split(":") {
        let dir_iterator = match fs::read_dir(dir) {
            Ok(iterator) => iterator,
            /* Check next directory */
            Err(_error) => continue,
        };

        /* Check each entry in the directory */
        for dir_entry in dir_iterator {
            let entry = match dir_entry {
                Ok(entry) => entry,
                Err(error) => return Err(error),
            };

            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(error) => return Err(error),
            };

            if file_type.is_file() && entry.file_name() == program {
                return Ok(Some(entry.path()));
            }
        }
    }

    /* Requested binary was not found */
    Ok(None)
}
