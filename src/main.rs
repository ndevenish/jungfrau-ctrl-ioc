use telnet::{Action, Event, Telnet, TelnetOption};
use std::io::{self, Write};
use tracing::debug;

fn main() {
    tracing_subscriber::fmt().with_max_level(tracing::Level::DEBUG).init();
    let mut connection = Telnet::connect(("i24-jf9mb-ctrl", 23), 256).unwrap();

    // Our initial burst of expectations
    for (action, option) in [
        (Action::Do, TelnetOption::SuppressGoAhead),
        (Action::Will, TelnetOption::TTYPE),
        (Action::Will, TelnetOption::NAWS),
        (Action::Wont, TelnetOption::NewEnvironment),
        (Action::Wont, TelnetOption::XDISPLOC),
    ] {
        debug!("CTRL: Sending {action:?} {option:?}");
        connection.negotiate(&action, option).unwrap();
    }
    let data = loop {
        let event = connection.read().expect("Read Error");
        if !matches!(event, Event::Data(_)) {
            debug!("CTRL: {event:?}");
        }

        // let mut handled_echo = false;
        match event {
            Event::Negotiation(Action::Do, option) => match option {
                TelnetOption::NAWS => {
                    debug!("CTRL:   └ Sending NAWS");
                    connection
                        .subnegotiate(TelnetOption::NAWS, &[0x00, 0x78, 0x00, 0x28])
                        .unwrap()
                }
                TelnetOption::Echo => {
                    let response = Action::Wont;
                    debug!("CTRL:   └ Responding {response:?}");
                    connection.negotiate(&response, option).unwrap()
                }
                TelnetOption::TSPEED => {
                    let response = Action::Will;
                    debug!("CTRL:   └ Responding {response:?}");
                    connection.negotiate(&response, option).unwrap()
                },
                TelnetOption::LFLOW => {
                    let response = Action::Wont;
                    debug!("CTRL:   └ Responding {response:?}");
                    connection.negotiate(&response, option).unwrap()
                },
                _ => (),
            },
            Event::Negotiation(Action::Will, option) => {
                let response = match option {
                    TelnetOption::Echo => Action::Do,
                    TelnetOption::Status => Action::Dont,
                    _ => continue,
                };

                // let response = match option {
                //     TelnetOption::SuppressGoAhead => Action::Do,
                //     TelnetOption::Echo => Action::Do,
                //     TelnetOption::TTYPE => Action::Do,
                //     _ => Action::Dont,
                // };
                debug!("CTRL:   └ Responding {response:?}");
                connection.negotiate(&response, option).unwrap()
            }
            // Event::Negotiation(Action::Wont)
            Event::Subnegotiation(TelnetOption::TTYPE, data) if data.get(0) == Some(&1) => {
                debug!("CTRL:   └ Responding TTYPE = xterm");
                connection
                    .subnegotiate(TelnetOption::TTYPE, "xterm".as_bytes())
                    .unwrap();
            }
            Event::Subnegotiation(TelnetOption::TSPEED, data) if data.get(0) == Some(&1) => {
                debug!("CTRL:   └ Responding TSPEED = 9600,9600");
                connection
                    .subnegotiate(TelnetOption::TSPEED, "38400,38400".as_bytes())
                    .unwrap();
            }
            Event::Data(d) => {
                // First data means we have finished the opening negotiations
                break d.into_vec();
            }
            _ => continue,
        }
    };
    print!("{}", String::from_utf8(data).unwrap());
    // Now we are past negotiation
    loop {
        let event = connection.read().expect("Read Error");
        let data = match event {
            Event::Data(d) => d.into_vec(),
            w => {
                print!("CTRL: {w:?}");
                continue;
            }
        };
        print!("{}", String::from_utf8(data).unwrap());
        io::stdout().flush().unwrap();
    }
}
