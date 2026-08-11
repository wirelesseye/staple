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
  const outputChannel = vscode.window.createOutputChannel("Staple");
  context.subscriptions.push(outputChannel);

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
      outputChannel,
    },
  );

  try {
    await client.start();
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    outputChannel.appendLine(`Failed to start the Staple language server: ${message}`);
    outputChannel.show(true);
    void vscode.window.showErrorMessage(
      `Failed to start the Staple language server: ${message}`,
    );
    throw error;
  }
}

async function deactivate() {
  if (client) {
    await client.stop();
  }
}

module.exports = { activate, deactivate };
