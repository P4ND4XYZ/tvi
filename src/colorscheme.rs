use ratatui::style::Style;

macro_rules! groups {
    ($($variant:ident => $name:literal),+ $(,)?) => {
        #[derive(Clone, Copy)]
        pub(crate) enum Group {
            $($variant),+
        }

        impl Group {
            const COUNT: usize = [$(Self::$variant),+].len();

            pub(crate) fn from_name(name: &str) -> Option<Self> {
                match name {
                    $($name => Some(Self::$variant),)+
                    _ => None,
                }
            }
        }
    };
}

groups! {
    DocumentNormal => "document.normal",
    Heading1 => "markdown.heading1",
    Heading2 => "markdown.heading2",
    Heading3 => "markdown.heading3",
    Heading4 => "markdown.heading4",
    Heading5 => "markdown.heading5",
    Heading6 => "markdown.heading6",
    Link => "markdown.link",
    Quote => "markdown.quote",
    Code => "markdown.code",
    SearchMatch => "search.match",
    SearchCurrent => "search.current",
    Status => "ui.status",
    Prompt => "ui.prompt",
    FloatNormal => "float.normal",
    FloatBorder => "float.border",
}

impl Group {
    pub(crate) fn heading(level: u16) -> Option<Self> {
        match level {
            1 => Some(Self::Heading1),
            2 => Some(Self::Heading2),
            3 => Some(Self::Heading3),
            4 => Some(Self::Heading4),
            5 => Some(Self::Heading5),
            6 => Some(Self::Heading6),
            _ => None,
        }
    }
}

#[derive(Default)]
pub(crate) struct Colorscheme {
    overrides: [Style; Group::COUNT],
}

impl Colorscheme {
    pub(crate) fn set(&mut self, group: Group, style: Style) {
        self.overrides[group as usize] = style;
    }

    pub(crate) fn apply(&self, group: Group, underlying: Style) -> Style {
        underlying.patch(self.overrides[group as usize])
    }
}
