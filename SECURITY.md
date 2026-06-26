# Security Policy

## Supported versions

aShot is pre-1.0 software. Security fixes are applied to the latest released
version and the `main` branch only.

| Version | Supported |
| ------- | --------- |
| latest  | ✅        |
| older   | ❌        |

## Reporting a vulnerability

Please report security issues privately rather than opening a public issue.

- Preferred: open a [GitHub private security advisory](https://github.com/lemon956/ashot/security/advisories/new).
- Alternatively, contact the maintainers through the repository's contact
  channels.

Include reproduction steps and the affected version or commit. We aim to
acknowledge reports within a few days.

## Data handling notes

aShot is a screenshot tool and handles potentially sensitive image content.
Reviewers and users should be aware of these data flows:

- **Local-first by default.** Captures use the system screenshot portal and are
  written to your configured save directory. The local OCR backend (Tesseract)
  does not upload images.
- **OCR.space online backend (opt-in).** When enabled in settings, the selected
  OCR region is uploaded to the OCR.space API and an API key is stored in the
  local config file. This backend is disabled by default; do not enable it for
  sensitive screenshots unless you accept the upload.
- **Clipboard.** Captures may be copied to the system clipboard, where other
  applications can read them.

When reporting issues, please redact any sensitive content from attached
screenshots.
