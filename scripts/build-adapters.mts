import {build} from 'esbuild';
for(const [entry,outfile]of [['ArchiveManager.mts','ArchiveManager.mjs'],['LocalCommandRunner.ts','LocalCommandRunner.js'],['bridge.mts','bridge.mjs']]) {
 await build({entryPoints:[entry],outfile:'dist/'+outfile,platform:'node',format:'esm',target:'node20',bundle:false});
}
await build({entryPoints:['editor.ts'],outfile:'dist/editor.js',platform:'browser',format:'iife',target:'es2020',bundle:true});
