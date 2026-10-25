import { cpSync, existsSync, mkdirSync, rmSync, copyFileSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { dirname, join, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";

const repo = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const npm = process.platform === "win32" ? "npm.cmd" : "npm";

const build = spawnSync(npm, ["run", "tauri", "--", "build", "--no-bundle"], {
  cwd: repo,
  stdio: "inherit",
  shell: process.platform === "win32",
});
if (build.error) {
  console.error(build.error.message);
  process.exit(1);
}
if (build.status !== 0) {
  process.exit(build.status ?? 1);
}

const exe = join(repo, "src-tauri", "target", "release", "lilith-artbank.exe");
if (!existsSync(exe)) {
  console.error(`找不到 release exe：${exe}`);
  process.exit(1);
}

const out = join(repo, "release", "Lilith Artbank");
mkdirSync(out, { recursive: true });
copyFileSync(exe, join(out, "lilith-artbank.exe"));

const serverTarget = join(out, "server");
rmSync(serverTarget, { recursive: true, force: true });
cpSync(join(repo, "server"), serverTarget, {
  recursive: true,
  filter: (source) => {
    const parts = source.split(sep);
    return !parts.includes("__pycache__") && !source.endsWith(".pyc");
  },
});

console.log(`\n便携目录已更新：${out}`);
console.log("请把 python_embeded\\ 与 resources\\models\\ 放进该目录后运行 lilith-artbank.exe。");
console.log("设置、日志与 AI 服务输出会生成在 exe 旁边。");
