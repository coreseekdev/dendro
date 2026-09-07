# 运维手册：本地 k8s 环境选型与已验证部署

> 场景：为 Dendro 的弹性调度设计（[负载自感知](../research/负载自感知与弹性调度.md)、
> [多节点 TP](../research/多节点TP事务调研.md)）提供本地验证环境。
> 本文记录各方案对比、本环境实测结论、以及一套**已验证可用**的部署。

## 1. 本地 k8s 实现对比

| 方案 | 形态 | 多节点模拟 | 前置要求 | 适合 | 不适合 |
|------|------|-----------|----------|------|--------|
| **kind** | k8s 跑在 Docker 容器里（kubeadm 引导）| ✅ 配置文件声明 N worker | 仅 Docker | Operator/CRD/多节点模拟、CI（k8s 官方 CI 同款）| 本沙箱实测踩坑（见 §2）|
| **k3d** | k3s 跑在 Docker 容器里 | ✅ | 仅 Docker | 快速开发循环、轻量 | 同属"嵌套 Docker 网络一类"，风险同 kind |
| **minikube** | 单机 VM/容器 + 丰富 addon | △（多机需多 VM）| Docker driver 可用 | 学习、单机全功能（dashboard/ingress 一键）| 多节点模拟不如 kind 直观 |
| **microk8s** | snap 系统级安装，组件直跑宿主 | ✅ `microk8s join`（真多机）| snapd + sudo | **长驻本机集群**、生产形态最接近 | 无 sudo/无 snapd 环境 |
| **k3s** | 单二进制系统服务 | ✅ join | sudo | 边缘/生产轻量 | 同上需 sudo |
| **kubeadm** | 生产标准引导 | ✅ | sudo + 规划 | 生产自建 | 本地实验过重 |
| 生产 | 托管（EKS/GKE/AKS）或 RKE2 | —— | —— | 上线 | ——

**关键教训（本环境实测）**：
- 选型第一步不是"哪个最好"，而是**有没有 root + Docker 网络行为是否正常**。
- kind/k3d 的节点是嵌套 netns：节点自连容器 IP 的行为依赖宿主 Docker 的网络栈。
  本环境（Docker 29 + 非常规内核）出现**容器自连 IP 被瞬间拒绝**的怪癖，
  kubeadm 健康检查（poll `https://<节点名>:6443`）必然超时——这与 kind 无关，
  换 k3d 大概率同样触发。
- 拿到 sudo 后改用 **microk8s**：组件直接跑宿主网络栈，完全绕开嵌套网络，
  一次装通。

## 2. 本环境已验证：microk8s 部署手册

### 2.1 安装（Ubuntu + snap）

```bash
sudo snap install microk8s --classic
sudo usermod -aG microk8s $USER     # 重新登录生效；或直接 sudo microk8s …
sudo microk8s status --wait-ready --timeout 240
```

### 2.2 镜像源（受限网络必配）

microk8s 的 containerd 直连 `registry.k8s.io` 会超时。两种解法（可叠加）：

**A. certs.d 镜像加速**（`/var/snap/microk8s/current/args/certs.d/`）：

```toml
# registry.k8s.io/hosts.toml
server = "https://registry.k8s.io"
[host."https://m.daocloud.io/registry.k8s.io"]
  capabilities = ["pull", "resolve"]
  override_path = true
```
> 实测 DaoCloud 对 HEAD 请求偶发 403——**更稳的做法是 B**。

**B. 手动拉取 + 重打标签**（确定性 100%）：

```bash
MK="sudo microk8s ctr images"
$MK pull registry.cn-hangzhou.aliyuncs.com/google_containers/<name>:<tag>
$MK tag  registry.cn-hangzhou.aliyuncs.com/google_containers/<name>:<tag> \
         registry.k8s.io/<name>:<tag>
```
（阿里云 `google_containers` 覆盖 pause/coredns/metrics-server 等 k8s 官方镜像；
calico 系在 docker.io，直连可达。）

### 2.3 已验证部署：dendro on microk8s

```bash
# 镜像（本地二进制直装，避开 Docker Hub auth 污染）
docker build -f Dockerfile.minimal -t dendro:local .
docker save dendro:local | sudo microk8s ctr images import -

# 部署（Deployment + NodePort 30543；yaml 见 /tmp/dendro-k8s.yaml 模板）
sudo microk8s kubectl apply -f /tmp/dendro-k8s.yaml
sudo microk8s kubectl get pods -l app=dendro        # 1/1 Running
```

**已验证用例**（tokio-postgres 真客户端打 NodePort 30543）：
建表 → 10 行插入 → CREATE BRANCH → 分支写入 → MERGE → CHECKPOINT →
跨连接验证 `count=11, sum=145` ✓

### 2.4 踩坑记录（供排障索引）

| 症状 | 根因 | 解法 |
|------|------|------|
| kind 建群卡 "Starting control-plane" 后 kubeadm 崩溃 | 节点内对 `<节点名>:6443` 的连接被瞬间拒绝（嵌套网络怪癖）| 本环境改用 microk8s；或修复宿主 Docker 网络 |
| microk8s calico 卡 Init / pause 拉不到 | registry.k8s.io 不可达 | §2.2 镜像源 |
| coredns ErrImagePull 403 | DaoCloud 对 HEAD 403 | 阿里云 google_containers 拉取 + retag（§2.2 B）|
| Pod Running 但 READY 0/1 | dendro 默认绑 127.0.0.1 | args 加 `--host 0.0.0.0` |
| **两表数据串台**（count 混入别的表的行）| `next_table_id` 从未递增，表 id 冲突 → memtx 串台（**已修复**，id 改为按 catalog 现存最大 id+1）| 升级后重建数据 |

## 3. 与弹性调度的衔接

本集群即为[负载自感知设计](../research/负载自感知与弹性调度.md)的执行器底座：

- **读副本 HPA**：Deployment（imagePullPolicy: Never）+ metrics-server 已就绪，
  HPA 直接可配
- **DendroOperator**：microk8s 支持 CRD + controller（helm3 addon 装 helm 后
  部署），分支租约转移 = Pod 间 endpoint 切换（P1 落地后接入）
- **RustFS**（Docker 容器 @19000）可与 microk8s Pod 共存：dendro Pod 用
  `--s3-endpoint http://172.20.0.1:19000`（网桥地址）即可把存储主体放 OSS，
  计算 Pod 完全无状态 → 驱逐/迁移/扩缩都只是"换个地方跑进程"
