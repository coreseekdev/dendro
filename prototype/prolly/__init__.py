"""prolly tree + 内容寻址 + git 式分支 原型验证包。

模块:
  hash.py      Hash20 内容地址 (sha512 前 20 字节, base32 文本编码)
  splitter.py  keySplitter 概率分裂 (weibull k=4, λ=4096)
  prolly.py    chunk 存储 / 节点编解码 / build-get-scan-iter / apply 增量修改 / diff
  branch.py    commit / catalog / create_branch / checkout / 三方 merge
  run_all.py   全部测试 + 统计摘要

运行方式:
  python3 -m prototype.prolly.run_all          # 在 dendro 仓库根目录
  python3 prototype/prolly/run_all.py          # 任意目录
  python3 prototype/prolly/<file>.py           # 单文件自测
"""
