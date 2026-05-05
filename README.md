# Language Server for Mago, for Zed Editor.

## What

I created a simplified LSP as a temporary solution until LSP is implemented in Mago v2 or later.

https://mago.carthage.software/latest/en/faq/#will-mago-implement-an-lsp

https://mago.carthage.software/latest/en/faq/#will-mago-offer-editor-extensions-vs-code-etc

## Features

- Diagnotics
  - linter
  - analyzer
- QuickFix


## Require

mago install and into your $PATH

https://mago.carthage.software/

## Setting

```
{
  "languages": {
    "PHP": {
      "language_servers": ["mago", "intelephense", "!phpactor", "!phptools"],
      "format_on_save": "on",
      "formatter": {
        "external": {
          "command": "mago",
          "arguments": ["format", "--stdin-input"],
        },
      },
    },
  },
}
```

## About the future

After Mago implements LSP, I plan to modify the code to use Mago's LSP.

Alternatively, I might submit a pull request for an official PHP extension.
