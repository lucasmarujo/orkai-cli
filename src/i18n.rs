use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum Language {
    #[default]
    En = 0,
    PtBr = 1,
}

static CURRENT_LANGUAGE: AtomicU8 = AtomicU8::new(Language::En as u8);

impl Language {
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim().to_lowercase();
        match s.as_str() {
            "pt" | "pt-br" | "pt_br" | "pt-pt" | "pt_pt" => Some(Language::PtBr),
            "en" | "en-us" | "en_us" | "en-gb" | "en_gb" => Some(Language::En),
            _ => None,
        }
    }

    pub fn code(&self) -> &'static str {
        match self {
            Language::En => "en",
            Language::PtBr => "pt-BR",
        }
    }

    pub fn is_pt(&self) -> bool {
        matches!(self, Language::PtBr)
    }

    /// Resolves the active language following the precedence order:
    /// 1. Command-line argument (`--lang <pt-br|en>`).
    /// 2. Config file (`language` in `config.json`).
    /// 3. OS locale / environment variables (`LANG`, `LC_ALL`, `LC_MESSAGES`).
    /// 4. Default fallback to English (`en`).
    pub fn resolve(cli_lang: Option<&str>, config_lang: Option<&str>) -> Self {
        // 1. Command-line argument
        if let Some(arg) = cli_lang
            && let Some(lang) = Self::parse(arg)
        {
            return lang;
        }

        // 2. Config file
        if let Some(cfg) = config_lang
            && let Some(lang) = Self::parse(cfg)
        {
            return lang;
        }

        // 3. System locale / environment variables
        if let Some(lang) = Self::detect_system_locale() {
            return lang;
        }

        // 4. Default fallback
        Language::En
    }

    pub fn detect_system_locale() -> Option<Self> {
        // Check standard POSIX environment variables
        for var in ["LC_ALL", "LC_MESSAGES", "LANG", "LC_CTYPE"] {
            if let Ok(val) = std::env::var(var)
                && !val.trim().is_empty()
                && let Some(lang) = Self::parse_system_locale_string(&val)
            {
                return Some(lang);
            }
        }

        // Check Windows system locale
        #[cfg(windows)]
        {
            if let Some(locale) = get_windows_locale()
                && let Some(lang) = Self::parse_system_locale_string(&locale)
            {
                return Some(lang);
            }
        }

        None
    }

    pub fn parse_system_locale_string(val: &str) -> Option<Self> {
        let val = val.trim().to_lowercase();
        if val.starts_with("pt") {
            Some(Language::PtBr)
        } else if val.starts_with("en")
            || val == "c"
            || val.starts_with("c.")
            || val == "posix"
            || val.starts_with("posix.")
        {
            Some(Language::En)
        } else {
            None
        }
    }
}

#[cfg(windows)]
fn get_windows_locale() -> Option<String> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetUserDefaultLocaleName(lpLocaleName: *mut u16, cchLocaleName: i32) -> i32;
    }

    let mut buf = [0u16; 85];
    let len = unsafe { GetUserDefaultLocaleName(buf.as_mut_ptr(), buf.len() as i32) };
    if len > 1 {
        let os_str = OsString::from_wide(&buf[..(len as usize - 1)]);
        return os_str.into_string().ok();
    }
    None
}

pub fn current_language() -> Language {
    match CURRENT_LANGUAGE.load(Ordering::Relaxed) {
        1 => Language::PtBr,
        _ => Language::En,
    }
}

pub fn set_current_language(lang: Language) {
    CURRENT_LANGUAGE.store(lang as u8, Ordering::Relaxed);
}

pub fn t<'a>(en: &'a str, pt: &'a str) -> &'a str {
    match current_language() {
        Language::PtBr => pt,
        Language::En => en,
    }
}

pub const HELP_EN: &[(&str, &str)] = &[
    ("type + enter", "start an agent with that task"),
    ("shift+enter", "new line in the task (alt+enter also works)"),
    ("ctrl+← · ctrl+→", "move the cursor by word in the task"),
    ("click on the task", "put the cursor where you clicked"),
    ("ctrl+↑ · ctrl+↓", "recall the tasks you sent in this session"),
    ("tab / shift+tab", "who runs it: an installed agent or terminal"),
    ("!command", "run a shell command in a new terminal"),
    ("/where · ctrl+l", "folder for this session: type, tab enters or cycles"),
    ("ctrl+t /worktree", "new agents in their own git worktree and branch"),
    ("/bypass", "new agents skip every permission prompt"),
    ("/diff · /merge", "see or merge the selected agent's worktree"),
    ("ctrl+p · /pwa", "your orkai on the phone, from anywhere (/pwa off turns it off)"),
    ("↑ ↓ / mouse", "select agent · click or enter opens it"),
    ("alt+1..9", "jump to an open tab"),
    ("alt+n", "open the next agent waiting for you (or finished)"),
    ("ctrl+]", "leave an agent and return to the overview"),
    ("shift+pgup/pgdn", "scroll an agent's history (or mouse wheel)"),
    ("ctrl+e / f2", "rename selected agent"),
    ("ctrl+x", "stop a running agent / remove a stopped one"),
    ("delete", "stop and remove the selected agent (asks first)"),
    ("ctrl+r", "recover interrupted agents of this session"),
    ("ctrl+n · alt+↑↓", "new session · switch session"),
    ("/", "commands: /cd /who /session /restart /recover …"),
    ("ctrl+c / ctrl+q", "quit — agents keep running in background"),
];

pub const HELP_PT_BR: &[(&str, &str)] = &[
    ("digite + enter", "inicia um agente com essa tarefa"),
    ("shift+enter", "nova linha na tarefa (alt+enter também funciona)"),
    ("ctrl+← · ctrl+→", "move o cursor por palavra na tarefa"),
    ("clique na tarefa", "posiciona o cursor onde você clicou"),
    ("ctrl+↑ · ctrl+↓", "recupera tarefas enviadas nesta sessão"),
    ("tab / shift+tab", "quem executa: um agente instalado ou terminal"),
    ("!comando", "executa comando shell em um novo terminal"),
    ("/where · ctrl+l", "pasta desta sessão: digite, tab entra ou navega"),
    ("ctrl+t /worktree", "novos agentes em seu próprio worktree e branch git"),
    ("/bypass", "novos agentes ignoram pedidos de permissão"),
    ("/diff · /merge", "visualiza ou mescla o worktree do agente selecionado"),
    ("ctrl+p · /pwa", "seu orkai no celular, de onde estiver (/pwa off desliga)"),
    ("↑ ↓ / mouse", "seleciona agente · clique ou enter abre"),
    ("alt+1..9", "vai para uma aba aberta"),
    ("alt+n", "abre o próximo agente aguardando você (ou finalizado)"),
    ("ctrl+]", "sai do agente e retorna à visão geral"),
    ("shift+pgup/pgdn", "rola o histórico do agente (ou roda do mouse)"),
    ("ctrl+e / f2", "renomeia o agente selecionado"),
    ("ctrl+x", "para agente em execução / remove agente parado"),
    ("delete", "para e remove o agente selecionado (pede confirmação)"),
    ("ctrl+r", "recupera agentes interrompidos desta sessão"),
    ("ctrl+n · alt+↑↓", "nova sessão · alternar sessão"),
    ("/", "comandos: /cd /who /session /restart /recover …"),
    ("ctrl+c / ctrl+q", "sair — agentes continuam rodando em segundo plano"),
];

pub fn help_title(lang: Language) -> &'static str {
    match lang {
        Language::En => " orkai · keys ",
        Language::PtBr => " orkai · atalhos ",
    }
}

pub fn help_items(lang: Language) -> &'static [(&'static str, &'static str)] {
    match lang {
        Language::En => HELP_EN,
        Language::PtBr => HELP_PT_BR,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_supported_language_codes() {
        assert_eq!(Language::parse("pt-BR"), Some(Language::PtBr));
        assert_eq!(Language::parse("pt-br"), Some(Language::PtBr));
        assert_eq!(Language::parse("pt_BR"), Some(Language::PtBr));
        assert_eq!(Language::parse("pt"), Some(Language::PtBr));
        assert_eq!(Language::parse("en"), Some(Language::En));
        assert_eq!(Language::parse("en-US"), Some(Language::En));
        assert_eq!(Language::parse("en_US"), Some(Language::En));
        assert_eq!(Language::parse("fr"), None);
        assert_eq!(Language::parse(""), None);
    }

    #[test]
    fn resolution_precedence() {
        // CLI arg wins over everything
        assert_eq!(Language::resolve(Some("pt-br"), Some("en")), Language::PtBr);
        assert_eq!(Language::resolve(Some("en"), Some("pt-BR")), Language::En);

        // Config wins when CLI arg is absent or invalid
        assert_eq!(Language::resolve(None, Some("pt-BR")), Language::PtBr);
        assert_eq!(Language::resolve(Some("invalid"), Some("pt-BR")), Language::PtBr);
        assert_eq!(Language::resolve(None, Some("en")), Language::En);

        // Fallback to English when none provided and env is empty
        assert_eq!(Language::resolve(None, None), Language::resolve(None, None));
    }

    #[test]
    fn translation_helper_respects_current_language() {
        set_current_language(Language::En);
        assert_eq!(t("hello", "olá"), "hello");

        set_current_language(Language::PtBr);
        assert_eq!(t("hello", "olá"), "olá");

        set_current_language(Language::En);
    }

    #[test]
    fn help_items_have_matching_counts() {
        assert_eq!(HELP_EN.len(), HELP_PT_BR.len());
        assert_eq!(HELP_EN.len(), 24);
    }

    #[test]
    fn parses_system_locale_strings() {
        assert_eq!(Language::parse_system_locale_string("pt_BR.UTF-8"), Some(Language::PtBr));
        assert_eq!(Language::parse_system_locale_string("pt-BR"), Some(Language::PtBr));
        assert_eq!(Language::parse_system_locale_string("en_US.UTF-8"), Some(Language::En));
        assert_eq!(Language::parse_system_locale_string("C"), Some(Language::En));
        assert_eq!(Language::parse_system_locale_string("C.UTF-8"), Some(Language::En));
        assert_eq!(Language::parse_system_locale_string("POSIX"), Some(Language::En));
        assert_eq!(Language::parse_system_locale_string("POSIX.UTF-8"), Some(Language::En));
        assert_eq!(Language::parse_system_locale_string("ja_JP.UTF-8"), None);
    }
}
