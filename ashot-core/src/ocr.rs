use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum OcrBackend {
    Tesseract,
    OcrSpace,
}

pub fn default_ocr_backend() -> OcrBackend {
    OcrBackend::Tesseract
}

pub fn default_ocr_languages() -> Vec<String> {
    vec!["chi_sim".to_string(), "eng".to_string()]
}

pub fn default_ocr_filter_symbols() -> bool {
    true
}

pub fn default_ocr_space_engine() -> u8 {
    2
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinuxDistroFamily {
    Debian,
    Fedora,
    Arch,
    OpenSuse,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OcrLanguagePackages {
    pub debian_ubuntu: &'static str,
    pub fedora: &'static str,
    pub arch: &'static str,
    pub opensuse: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OcrLanguage {
    pub display_name: &'static str,
    pub tesseract_code: &'static str,
    pub ocr_space_code: &'static str,
    pub keywords: &'static [&'static str],
    pub packages: OcrLanguagePackages,
}

pub const OCR_LANGUAGES: &[OcrLanguage] = &[
    OcrLanguage {
        display_name: "Auto Detect / 自动",
        tesseract_code: "auto",
        ocr_space_code: "auto",
        keywords: &["auto", "detect", "automatic", "自动", "自動"],
        packages: OcrLanguagePackages { debian_ubuntu: "", fedora: "", arch: "", opensuse: "" },
    },
    OcrLanguage {
        display_name: "Chinese Simplified / 简体中文",
        tesseract_code: "chi_sim",
        ocr_space_code: "chs",
        keywords: &["chinese", "simplified", "中文", "简体", "mandarin", "chi_sim", "chs"],
        packages: OcrLanguagePackages {
            debian_ubuntu: "tesseract-ocr-chi-sim",
            fedora: "tesseract-langpack-chi_sim",
            arch: "tesseract-data-chi_sim",
            opensuse: "tesseract-ocr-traineddata-chinese-simplified",
        },
    },
    OcrLanguage {
        display_name: "English / 英文",
        tesseract_code: "eng",
        ocr_space_code: "eng",
        keywords: &["english", "英文", "英语", "eng"],
        packages: OcrLanguagePackages {
            debian_ubuntu: "tesseract-ocr-eng",
            fedora: "tesseract-langpack-eng",
            arch: "tesseract-data-eng",
            opensuse: "tesseract-ocr-traineddata-english",
        },
    },
    OcrLanguage {
        display_name: "Chinese Traditional / 繁體中文",
        tesseract_code: "chi_tra",
        ocr_space_code: "cht",
        keywords: &["chinese", "traditional", "中文", "繁体", "繁體", "chi_tra", "cht"],
        packages: OcrLanguagePackages {
            debian_ubuntu: "tesseract-ocr-chi-tra",
            fedora: "tesseract-langpack-chi_tra",
            arch: "tesseract-data-chi_tra",
            opensuse: "tesseract-ocr-traineddata-chinese-traditional",
        },
    },
    OcrLanguage {
        display_name: "Japanese / 日本語",
        tesseract_code: "jpn",
        ocr_space_code: "jpn",
        keywords: &["japanese", "日本", "日文", "日语", "jpn"],
        packages: OcrLanguagePackages {
            debian_ubuntu: "tesseract-ocr-jpn",
            fedora: "tesseract-langpack-jpn",
            arch: "tesseract-data-jpn",
            opensuse: "tesseract-ocr-traineddata-japanese",
        },
    },
    OcrLanguage {
        display_name: "Korean / 한국어",
        tesseract_code: "kor",
        ocr_space_code: "kor",
        keywords: &["korean", "韩国", "韩文", "韩语", "韓文", "kor"],
        packages: OcrLanguagePackages {
            debian_ubuntu: "tesseract-ocr-kor",
            fedora: "tesseract-langpack-kor",
            arch: "tesseract-data-kor",
            opensuse: "tesseract-ocr-traineddata-korean",
        },
    },
    OcrLanguage {
        display_name: "French / Français",
        tesseract_code: "fra",
        ocr_space_code: "fre",
        keywords: &["french", "français", "法文", "法语", "fra", "fre"],
        packages: OcrLanguagePackages {
            debian_ubuntu: "tesseract-ocr-fra",
            fedora: "tesseract-langpack-fra",
            arch: "tesseract-data-fra",
            opensuse: "tesseract-ocr-traineddata-french",
        },
    },
    OcrLanguage {
        display_name: "German / Deutsch",
        tesseract_code: "deu",
        ocr_space_code: "ger",
        keywords: &["german", "deutsch", "德文", "德语", "deu", "ger"],
        packages: OcrLanguagePackages {
            debian_ubuntu: "tesseract-ocr-deu",
            fedora: "tesseract-langpack-deu",
            arch: "tesseract-data-deu",
            opensuse: "tesseract-ocr-traineddata-german",
        },
    },
    OcrLanguage {
        display_name: "Spanish / Español",
        tesseract_code: "spa",
        ocr_space_code: "spa",
        keywords: &["spanish", "español", "西班牙", "spa"],
        packages: OcrLanguagePackages {
            debian_ubuntu: "tesseract-ocr-spa",
            fedora: "tesseract-langpack-spa",
            arch: "tesseract-data-spa",
            opensuse: "tesseract-ocr-traineddata-spanish",
        },
    },
    OcrLanguage {
        display_name: "Russian / Русский",
        tesseract_code: "rus",
        ocr_space_code: "rus",
        keywords: &["russian", "русский", "俄文", "俄语", "rus"],
        packages: OcrLanguagePackages {
            debian_ubuntu: "tesseract-ocr-rus",
            fedora: "tesseract-langpack-rus",
            arch: "tesseract-data-rus",
            opensuse: "tesseract-ocr-traineddata-russian",
        },
    },
    OcrLanguage {
        display_name: "Vietnamese / Tiếng Việt",
        tesseract_code: "vie",
        ocr_space_code: "vie",
        keywords: &["vietnamese", "越南", "vie"],
        packages: OcrLanguagePackages {
            debian_ubuntu: "tesseract-ocr-vie",
            fedora: "tesseract-langpack-vie",
            arch: "tesseract-data-vie",
            opensuse: "tesseract-ocr-traineddata-vietnamese",
        },
    },
];

pub fn search_ocr_languages(query: &str) -> Vec<&'static OcrLanguage> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return OCR_LANGUAGES.iter().collect();
    }

    OCR_LANGUAGES
        .iter()
        .filter(|language| {
            language.display_name.to_lowercase().contains(&query)
                || language.tesseract_code.contains(&query)
                || language.ocr_space_code.contains(&query)
                || language.keywords.iter().any(|keyword| keyword.to_lowercase().contains(&query))
        })
        .collect()
}

pub fn ocr_language_by_tesseract_code(code: &str) -> Option<&'static OcrLanguage> {
    OCR_LANGUAGES.iter().find(|language| language.tesseract_code == code)
}

pub fn language_package_for_distro(
    language: &OcrLanguage,
    distro: LinuxDistroFamily,
) -> Option<&'static str> {
    match distro {
        LinuxDistroFamily::Debian => non_empty_package(language.packages.debian_ubuntu),
        LinuxDistroFamily::Fedora => non_empty_package(language.packages.fedora),
        LinuxDistroFamily::Arch => non_empty_package(language.packages.arch),
        LinuxDistroFamily::OpenSuse => non_empty_package(language.packages.opensuse),
        LinuxDistroFamily::Unknown => None,
    }
}

fn non_empty_package(package: &'static str) -> Option<&'static str> {
    if package.is_empty() { None } else { Some(package) }
}

pub fn language_install_command(codes: &[String], distro: LinuxDistroFamily) -> String {
    let install_codes = language_codes_for_install(codes);
    let languages = install_codes
        .iter()
        .filter_map(|code| ocr_language_by_tesseract_code(code))
        .collect::<Vec<_>>();

    if distro == LinuxDistroFamily::Unknown {
        let codes = install_codes.join("+");
        return format!("Install tesseract and the {codes} traineddata language pack");
    }

    let packages = languages
        .iter()
        .filter_map(|language| language_package_for_distro(language, distro))
        .collect::<Vec<_>>();

    match distro {
        LinuxDistroFamily::Debian => {
            format!("sudo apt install tesseract-ocr {}", packages.join(" "))
        }
        LinuxDistroFamily::Fedora => format!("sudo dnf install tesseract {}", packages.join(" ")),
        LinuxDistroFamily::Arch => format!("sudo pacman -S tesseract {}", packages.join(" ")),
        LinuxDistroFamily::OpenSuse => {
            format!("sudo zypper install tesseract-ocr {}", packages.join(" "))
        }
        LinuxDistroFamily::Unknown => unreachable!(),
    }
}

fn language_codes_for_install(codes: &[String]) -> Vec<String> {
    if codes.is_empty() || codes.iter().any(|code| code == "auto") {
        default_ocr_languages()
    } else {
        codes.to_vec()
    }
}

pub fn linux_distro_family_from_os_release(content: &str) -> LinuxDistroFamily {
    let mut tokens = Vec::new();
    for line in content.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key != "ID" && key != "ID_LIKE" {
            continue;
        }
        let value = value.trim().trim_matches('"').trim_matches('\'');
        tokens.extend(value.split_whitespace().map(|item| item.to_lowercase()));
    }

    if tokens.iter().any(|item| matches!(item.as_str(), "debian" | "ubuntu" | "linuxmint")) {
        return LinuxDistroFamily::Debian;
    }
    if tokens.iter().any(|item| matches!(item.as_str(), "fedora" | "rhel" | "centos")) {
        return LinuxDistroFamily::Fedora;
    }
    if tokens.iter().any(|item| matches!(item.as_str(), "arch" | "manjaro")) {
        return LinuxDistroFamily::Arch;
    }
    if tokens.iter().any(|item| matches!(item.as_str(), "opensuse" | "suse" | "sles")) {
        return LinuxDistroFamily::OpenSuse;
    }
    LinuxDistroFamily::Unknown
}

pub fn detect_linux_distro_family() -> LinuxDistroFamily {
    std::fs::read_to_string("/etc/os-release")
        .map(|content| linux_distro_family_from_os_release(&content))
        .unwrap_or(LinuxDistroFamily::Unknown)
}

#[cfg(test)]
mod tests {
    use super::{
        LinuxDistroFamily, language_install_command, language_package_for_distro,
        linux_distro_family_from_os_release, ocr_language_by_tesseract_code, search_ocr_languages,
    };

    #[test]
    fn language_search_matches_names_and_codes() {
        assert_eq!(search_ocr_languages("auto")[0].tesseract_code, "auto");
        assert_eq!(search_ocr_languages("中文")[0].tesseract_code, "chi_sim");
        assert_eq!(search_ocr_languages("english")[0].tesseract_code, "eng");
        assert_eq!(search_ocr_languages("jpn")[0].tesseract_code, "jpn");
    }

    #[test]
    fn distro_package_names_are_language_specific() {
        let language = ocr_language_by_tesseract_code("chi_sim").expect("language");

        assert_eq!(
            language_package_for_distro(language, LinuxDistroFamily::Debian),
            Some("tesseract-ocr-chi-sim")
        );
        assert_eq!(
            language_package_for_distro(language, LinuxDistroFamily::Fedora),
            Some("tesseract-langpack-chi_sim")
        );
        assert_eq!(
            language_package_for_distro(language, LinuxDistroFamily::Arch),
            Some("tesseract-data-chi_sim")
        );
    }

    #[test]
    fn selected_languages_generate_full_install_command() {
        let command = language_install_command(
            &["chi_sim".to_string(), "eng".to_string()],
            LinuxDistroFamily::Debian,
        );

        assert_eq!(
            command,
            "sudo apt install tesseract-ocr tesseract-ocr-chi-sim tesseract-ocr-eng"
        );
    }

    #[test]
    fn auto_language_generates_default_local_install_command() {
        let command = language_install_command(&["auto".to_string()], LinuxDistroFamily::Debian);

        assert_eq!(
            command,
            "sudo apt install tesseract-ocr tesseract-ocr-chi-sim tesseract-ocr-eng"
        );
    }

    #[test]
    fn unknown_distro_generates_plain_traineddata_hint() {
        let command = language_install_command(&["jpn".to_string()], LinuxDistroFamily::Unknown);

        assert!(command.contains("Install tesseract and the jpn traineddata language pack"));
    }

    #[test]
    fn distro_detection_uses_id_and_id_like() {
        assert_eq!(
            linux_distro_family_from_os_release("ID=ubuntu\nID_LIKE=debian\n"),
            LinuxDistroFamily::Debian
        );
        assert_eq!(
            linux_distro_family_from_os_release("ID=silverblue\nID_LIKE=\"fedora rhel\"\n"),
            LinuxDistroFamily::Fedora
        );
        assert_eq!(linux_distro_family_from_os_release("ID=arch\n"), LinuxDistroFamily::Arch);
    }
}
