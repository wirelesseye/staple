const vscode = require("vscode");
const {
  LanguageClient,
  TransportKind,
} = require("vscode-languageclient/node");

let client;

async function activate(context) {
  const configuration = vscode.workspace.getConfiguration("staple");
  const command = configuration.get("languageServer.path", "staple-lsp");
  const standardLibrary = configuration.get("standardLibrary.path", "");
  const args = standardLibrary ? ["--stdlib", standardLibrary] : [];

  const watcher = vscode.workspace.createFileSystemWatcher("**/*.sta");
  context.subscriptions.push(watcher);

  client = new LanguageClient(
    "staple",
    "Staple Language Server",
    { command, args, transport: TransportKind.stdio },
    {
      documentSelector: [{ scheme: "file", language: "staple" }],
      synchronize: {
        configurationSection: "staple",
        fileEvents: watcher,
      },
    },
  );

  await client.start();
}

async function deactivate() {
  if (client) {
    await client.stop();
  }
}

module.exports = { activate, deactivate };
