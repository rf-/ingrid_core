import * as ingridCore from "./pkg/ingrid_core.js";
import { promisify } from 'node:util';

// Removed unused import as Deno.readTextFile is used instead
// Load the word list from disk
// const wordList = await Deno.readTextFile('./resources/spreadthewordlist.dict');
// console.log("Word list loaded successfully from disk:", wordList.slice(0, 100));

const testUrl = 'http://localhost:8080/spreadthewordlist.dict';
const testFilePath = './resources/spreadthewordlist.dict';
const gridFilePath = './resources/emptyGrid.txt';

const gridContent = 
".....\n" +
".....\n" +
".....\n" +
".....\n" +
".....";

async function testGridFill() {
    try {
        console.log("Initializing WebAssembly module...");
        await ingridCore.default();
        
        console.log("Creating grid content...");

        // Call fill_grid with only the grid template, letting other parameters be null
        const result = await ingridCore.fill_grid(gridContent, null, null);
        console.log("Grid filling succeeded:", result);
    } catch (error) {
        console.error("Error during grid fill:", error);
        if (error instanceof Error) {
            console.error("Error name:", error.name);
            console.error("Error message:", error.message);
            console.error("Error stack:", error.stack);
        }
    }
}

// New test case that loads a wordlist from disk and passes it to fill_grid
async function testGridFillWithWordList() {
    try {
        console.log("Initializing WebAssembly module...");
        await ingridCore.default();
        
        console.log("Creating grid content...");
        
        // Call fill_grid with the loaded word list as final argument
        const result = await ingridCore.fill_grid(gridContent, null, null, testUrl);
        console.log("Grid filling with word list succeeded:", result);
    } catch (error) {
        console.error("Error during grid fill with word list:", error);
        if (error instanceof Error) {
            console.error("Error name:", error.name);
            console.error("Error message:", error.message);
            console.error("Error stack:", error.stack);
        }
    }
}

// New test case to compare runtimes
async function testRuntimeComparison() {
    try {
        console.log("Comparing runtimes for ingridCore.fill_grid() vs CLI");
        
        // Add WebAssembly module initialization
        console.log("Initializing WebAssembly module...");
        await ingridCore.default();
        
        // Measure runtime for ingridCore.fill_grid()
        const startWasm = Date.now();
        await ingridCore.fill_grid(gridContent, null, null, testUrl);
        const wasmRuntime = Date.now() - startWasm;
        console.log("WASM runtime:", wasmRuntime, "ms");
        
        // Prepare CLI command; replace newlines for safe CLI argument passing
        const gridArg = gridContent.replace(/\n/g, ' ');
        // Assuming the CLI accepts parameters: --grid "<gridContent>" --wordlist <wordlist>
        const cliCommand = `ingrid_core --wordlist ${testFilePath} ${gridFilePath}`;
        
        const startCli = Date.now();
        const parts = cliCommand.split(" ");
        const command = new Deno.Command(parts[0], {
            args: parts.slice(1),
            stdout: "piped",
            stderr: "piped",
        });
        const output = await command.output();

        const { code, stdout, stderr } = output;
        const stdoutText = new TextDecoder().decode(stdout);
        const stderrText = new TextDecoder().decode(stderr);

        if (code !== 0) {
            throw new Error(`CLI process exited with code ${code}\nError output: ${stderrText}`);
        }
        const cliRuntime = Date.now() - startCli;
        console.log("CLI runtime:", cliRuntime, "ms");
        console.log("CLI output:", stdoutText);
        if (stderrText) console.error("CLI error output:", stderrText);
    } catch (error) {
        console.error("Error during runtime comparison:", error);
    }
}

// Run the tests sequentially
await testGridFill();
await testGridFillWithWordList();
await testRuntimeComparison();
