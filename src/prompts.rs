//! Start each interactive prompt on a clean terminal screen.
use std::fmt::Display;
use std::io;

pub fn select<T: Clone + Eq>(title: impl Display) -> io::Result<cliclack::Select<T>> {
    cliclack::clear_screen()?;
    Ok(cliclack::select(title))
}

pub fn input(title: impl Display) -> io::Result<cliclack::Input> {
    cliclack::clear_screen()?;
    Ok(cliclack::input(title))
}

pub fn multiselect<T: Clone + Eq>(title: impl Display) -> io::Result<cliclack::MultiSelect<T>> {
    cliclack::clear_screen()?;
    Ok(cliclack::multiselect(title))
}
