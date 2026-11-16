# Recovery 内置 Root 资源

打包 recovery 时，`tools/pack-recovery.sh` 会将本目录（或 `SKYWARD_ROOT_BUNDLE` 指向的目录）中的安装包写入 ramdisk 的 `skyward/root/`。

请将以下文件放到本目录（或你的 bundle 目录），文件名与 `manifest.toml` 一致：

- `KernelSU_v3.3.1_32680-release.apk`
- `APatch_6ec140e.apk`
- `ReSukiSU_v4.2.0-rc3_35171-arm64-v8a-release.zip`

另：带 `META-INF/com/google/android/update-binary` 的 **刷机 zip** 可在 `manifest.toml` 中设 `method = "flash"`，在 recovery 里走安装流程。