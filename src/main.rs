use telnet::{Telnet, TelnetOption, Action, Event};

fn main() {
    let mut connection = Telnet::connect(("i24-jf9mb-ctrl", 23), 256).unwrap();
    connection.negotiate(&Action::Wont, TelnetOption::TTYPE).unwrap();
    connection.negotiate(&Action::Wont, TelnetOption::TSPEED).unwrap();
    connection.negotiate(&Action::Wont, TelnetOption::XDISPLOC).unwrap();
    connection.negotiate(&Action::Wont, TelnetOption::NewEnvironment).unwrap();
//     IAC WONT TTYPE
// IAC WONT TSPEED
// IAC WONT XDISPLOC
// IAC WONT NEW-ENVIRON
    let data = loop {
        let event = connection.read().expect("Read Error");
        if ! matches!(event, Event::Data(_)) {
            println!("CTRL: {event:?}");
        }
        match event {
            Event::Negotiation(Action::Do, option) => connection.negotiate(&match option {TelnetOption::Echo => Action::Wont}, TelnetOption::Echo).unwrap(),
            Event::Negotiation(Action::Do, TelnetOption::Echo) => connection.negotiate(&Action::Wont, TelnetOption::Echo).unwrap(),

            Event::Negotiation(Action::Do, TelnetOption::TTYPE) => connection.negotiate(&Action::Will, TelnetOption::TTYPE).unwrap(),
            Event::Negotiation(Action::Do, TelnetOption::NAWS) => connection.negotiate(&Action::Will, TelnetOption::NAWS).unwrap(),

            Event::Negotiation(Action::Do, option) => connection.negotiate(&Action::Wont, option).unwrap(),
            Event::Negotiation(Action::Will, TelnetOption::SuppressGoAhead) => connection.negotiate(&Action::Do, TelnetOption::SuppressGoAhead).unwrap(),
            Event::Negotiation(Action::Will, option) => connection.negotiate(&Action::Do, option).unwrap(),

            Event::Data(d) => {
                break d.into_vec();
            }
            w => println!("Got unhandled: {w:?}"),
        }
    };
    println!("{}", String::from_utf8(data).unwrap());
    // Now we are past negotiation
    loop {
        let event = connection.read().expect("Read Error");
        let data = match event {
            Event::Data(d) => d.into_vec(),
            w => {
                println!("CTRL: {w:?}");
                continue;
            },
        };
        print!("{}", String::from_utf8(data).unwrap());
    }
}
