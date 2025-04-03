import * as ingridCore from "./pkg/ingrid_core.js";

// Removed unused import as Deno.readTextFile is used instead
// Load the word list from disk
// const wordList = await Deno.readTextFile('./resources/spreadthewordlist.dict');
// console.log("Word list loaded successfully from disk:", wordList.slice(0, 100));

const testUrl = 'http://localhost:8000/resources/XwiWordList.txt';
const testFilePath = './resources/XwiWordList.txt';
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
        
        // Measure WebAssembly initialization time
        const startWasmInit = Date.now();
        console.log("Initializing WebAssembly module...");
        await ingridCore.default();
        const wasmInitTime = Date.now() - startWasmInit;
        console.log("WASM initialization time:", wasmInitTime, "ms");
            
        // Measure just the grid filling time
        const startWasmFill = Date.now();
        // await ingridCore.fill_grid(gridContent, null, null, testUrl);
        await ingridCore.fill_grid(gridContent, null, null);
        const wasmFillTime = Date.now() - startWasmFill;
        console.log("WASM grid fill time:", wasmFillTime, "ms");
        console.log("WASM total time:", wasmInitTime + wasmFillTime, "ms");
        
        // CLI timing remains the same
        const cliCommand = `ingrid_core --wordlist ${testFilePath} ${gridFilePath}`;
        
        const startCli = Date.now();
        const parts = cliCommand.split(" ");
        const command = new Deno.Command(parts[0], {
            args: parts.slice(1),
            stdout: "piped",
            stderr: "piped",
        });
        const output = await command.output();
        const cliRuntime = Date.now() - startCli;
        console.log("CLI runtime:", cliRuntime, "ms");
        
        // Process output
        const textDecoder = new TextDecoder();
        const stdout = textDecoder.decode(output.stdout);
        console.log("CLI output:", stdout);
        const stderr = textDecoder.decode(output.stderr);
        if (stderr) console.error("CLI error output:", stderr);
    } catch (error) {
        console.error("Error during runtime comparison:", error);
    }
}

// Run the tests sequentially
await testGridFill();
await testGridFillWithWordList();
await testRuntimeComparison();
