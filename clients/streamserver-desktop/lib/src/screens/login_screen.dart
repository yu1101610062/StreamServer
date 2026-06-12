import 'dart:math' as math;

import 'package:flutter/material.dart';
import 'package:lucide_icons_flutter/lucide_icons.dart';

import '../core/theme/stream_theme.dart';
import '../state.dart';

class LoginScreen extends StatefulWidget {
  const LoginScreen({super.key});

  @override
  State<LoginScreen> createState() => _LoginScreenState();
}

class _LoginScreenState extends State<LoginScreen> {
  final serverController = TextEditingController(text: 'http://127.0.0.1:8080');
  final usernameController = TextEditingController(text: 'admin');
  final passwordController = TextEditingController();
  final manualHostController = TextEditingController(text: '172.17.13.196');
  final manualPortController = TextEditingController(text: '8080');
  final passwordFocusNode = FocusNode();
  String manualProtocol = 'http';
  List<Map<String, Object?>> discoveredServers = const [];
  bool scanning = false;
  bool probing = false;
  String scanStatus = '';
  String? error;

  @override
  void dispose() {
    serverController.dispose();
    usernameController.dispose();
    passwordController.dispose();
    manualHostController.dispose();
    manualPortController.dispose();
    passwordFocusNode.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final controller = AppScope.of(context);
    final profiles = controller.serverProfiles;
    final colors = context.streamColors;
    return Scaffold(
      backgroundColor: colors.appBackground,
      body: LayoutBuilder(
        builder: (context, constraints) {
          final compact = constraints.maxWidth < 560;
          final outerPadding = compact ? 12.0 : 24.0;
          final cardPadding = compact ? 18.0 : 28.0;
          return DecoratedBox(
            decoration: BoxDecoration(color: colors.appBackground),
            child: SafeArea(
              child: SingleChildScrollView(
                padding: EdgeInsets.all(outerPadding),
                child: ConstrainedBox(
                  constraints: BoxConstraints(
                    minHeight:
                        math.max(0, constraints.maxHeight - outerPadding * 2),
                  ),
                  child: Center(
                    child: ConstrainedBox(
                      constraints: const BoxConstraints(maxWidth: 880),
                      child: DecoratedBox(
                        decoration: BoxDecoration(
                          color: colors.surface,
                          border: Border.all(color: colors.border),
                          borderRadius: BorderRadius.circular(14),
                          boxShadow: [
                            BoxShadow(
                              color: Colors.black.withValues(alpha: 0.28),
                              blurRadius: 28,
                              offset: const Offset(0, 18),
                            ),
                          ],
                        ),
                        child: Padding(
                          padding: EdgeInsets.all(cardPadding),
                          child: DefaultTextStyle(
                            style: TextStyle(color: colors.textPrimary),
                            child: Column(
                              mainAxisSize: MainAxisSize.min,
                              crossAxisAlignment: CrossAxisAlignment.start,
                              children: [
                                Text(
                                  'STREAMSERVER',
                                  style: TextStyle(
                                    color: colors.primary,
                                    fontWeight: FontWeight.w800,
                                    letterSpacing: 1.2,
                                  ),
                                ),
                                const SizedBox(height: 8),
                                Text(
                                  '桌面控制台',
                                  style: Theme.of(context)
                                      .textTheme
                                      .headlineMedium
                                      ?.copyWith(
                                        color: colors.textPrimary,
                                        fontWeight: FontWeight.w900,
                                      ),
                                ),
                                const SizedBox(height: 24),
                                TextField(
                                  controller: serverController,
                                  textInputAction: TextInputAction.next,
                                  decoration: const InputDecoration(
                                    labelText: 'Core 地址',
                                    prefixIcon: Icon(LucideIcons.server),
                                  ),
                                ),
                                if (profiles.isNotEmpty) ...[
                                  const SizedBox(height: 8),
                                  DropdownButtonFormField<String>(
                                    initialValue: profiles.any((item) =>
                                            item.baseUrl ==
                                            serverController.text)
                                        ? serverController.text
                                        : null,
                                    decoration: const InputDecoration(
                                      labelText: '已保存服务器',
                                      prefixIcon: Icon(LucideIcons.database),
                                    ),
                                    items: profiles
                                        .map((profile) => DropdownMenuItem(
                                              value: profile.baseUrl,
                                              child: Text(
                                                profile.name,
                                                maxLines: 1,
                                                overflow: TextOverflow.ellipsis,
                                              ),
                                            ))
                                        .toList(),
                                    onChanged: (value) {
                                      if (value != null) {
                                        final profile = profiles.firstWhere(
                                            (item) => item.baseUrl == value);
                                        controller.selectServer(profile);
                                        serverController.text = profile.baseUrl;
                                      }
                                    },
                                  ),
                                ],
                                const SizedBox(height: 12),
                                _DiscoveryPanel(
                                  scanning: scanning,
                                  probing: probing,
                                  status: scanStatus,
                                  results: discoveredServers,
                                  manualProtocol: manualProtocol,
                                  manualHostController: manualHostController,
                                  manualPortController: manualPortController,
                                  onProtocolChanged: (value) =>
                                      setState(() => manualProtocol = value),
                                  onScan: () => _scan(controller),
                                  onSelect: (baseUrl) => setState(
                                      () => serverController.text = baseUrl),
                                  onProbe: () => _probe(controller),
                                ),
                                const SizedBox(height: 12),
                                TextField(
                                  controller: usernameController,
                                  textInputAction: TextInputAction.next,
                                  decoration: const InputDecoration(
                                    labelText: '用户名',
                                    prefixIcon: Icon(LucideIcons.user),
                                  ),
                                  onSubmitted: (_) =>
                                      passwordFocusNode.requestFocus(),
                                ),
                                const SizedBox(height: 12),
                                TextField(
                                  controller: passwordController,
                                  focusNode: passwordFocusNode,
                                  obscureText: true,
                                  textInputAction: TextInputAction.done,
                                  decoration: const InputDecoration(
                                    labelText: '密码',
                                    prefixIcon: Icon(LucideIcons.lock),
                                  ),
                                  onSubmitted: (_) => _submitLogin(controller),
                                ),
                                if (error != null) ...[
                                  const SizedBox(height: 12),
                                  Text(error!,
                                      style: TextStyle(color: colors.danger)),
                                ],
                                const SizedBox(height: 20),
                                SizedBox(
                                  width: double.infinity,
                                  child: FilledButton.icon(
                                    onPressed: controller.busy
                                        ? null
                                        : () => _submitLogin(controller),
                                    icon: controller.busy
                                        ? const SizedBox.square(
                                            dimension: 16,
                                            child: CircularProgressIndicator(
                                                strokeWidth: 2),
                                          )
                                        : const Icon(LucideIcons.logIn),
                                    label: const Text('连接并登录'),
                                  ),
                                ),
                              ],
                            ),
                          ),
                        ),
                      ),
                    ),
                  ),
                ),
              ),
            ),
          );
        },
      ),
    );
  }

  Future<void> _submitLogin(AppController controller) async {
    if (controller.busy) return;
    await _login(controller);
  }

  Future<void> _login(AppController controller) async {
    try {
      await controller.login(
        baseUrl: serverController.text,
        username: usernameController.text,
        password: passwordController.text,
      );
    } catch (cause) {
      setState(() {
        error = cause.toString();
      });
    }
  }

  Future<void> _scan(AppController controller) async {
    setState(() {
      scanning = true;
      error = null;
      discoveredServers = const [];
      scanStatus = '正在优先探测手动地址和已保存服务器';
    });
    try {
      final quick = await _probeManual(controller);
      if (quick != null && mounted) {
        final baseUrl = quick['base_url'] as String;
        setState(() {
          serverController.text = baseUrl;
          discoveredServers = [quick];
          scanStatus = '已发现 $baseUrl，继续扫描相关网段';
        });
      }
      final results = await controller.scanServers(
        baseUrls: _scanBaseUrls(controller),
        seedHosts: _scanSeedHosts(),
      );
      setState(() {
        discoveredServers =
            _mergeDiscoveryResults([...discoveredServers, ...results]);
        scanStatus = discoveredServers.isEmpty
            ? ''
            : '扫描完成，发现 ${discoveredServers.length} 个实例';
        if (discoveredServers.isEmpty) {
          error = '未扫描到 StreamServer Core，可使用手动连接。';
        }
      });
    } catch (cause) {
      setState(() => error = cause.toString());
    } finally {
      if (mounted) setState(() => scanning = false);
    }
  }

  Future<void> _probe(AppController controller) async {
    setState(() {
      probing = true;
      error = null;
      scanStatus = '正在探测手动地址';
    });
    try {
      final item = await _probeManual(controller);
      if (item == null) {
        return;
      }
      final baseUrl = item['base_url'] as String;
      setState(() {
        serverController.text = baseUrl;
        discoveredServers =
            _mergeDiscoveryResults([item, ...discoveredServers]);
        scanStatus = '已发现 $baseUrl';
      });
    } catch (cause) {
      setState(() => error = cause.toString());
    } finally {
      if (mounted) setState(() => probing = false);
    }
  }

  Future<Map<String, Object?>?> _probeManual(AppController controller) async {
    final port = int.tryParse(manualPortController.text) ?? 8080;
    final result = await controller.probeServer(
      protocol: manualProtocol,
      host: manualHostController.text,
      port: port,
    );
    final found = result['found'] == true;
    if (!found) {
      final detail = result['error'] as String?;
      if (mounted) {
        setState(() => error = detail == null || detail.isEmpty
            ? '该地址未识别为 StreamServer Core'
            : '探测失败：$detail');
      }
      return null;
    }
    return (result['item'] as Map).cast<String, Object?>();
  }

  List<String> _scanBaseUrls(AppController controller) {
    final values = <String>{
      serverController.text.trim(),
      '${manualProtocol.trim()}://${manualHostController.text.trim()}:${int.tryParse(manualPortController.text) ?? 8080}',
      ...controller.serverProfiles.map((profile) => profile.baseUrl.trim()),
    };
    return values
        .where((value) =>
            value.startsWith('http://') || value.startsWith('https://'))
        .toList();
  }

  List<String> _scanSeedHosts() {
    final host = manualHostController.text.trim();
    return host.isEmpty ? const [] : [host];
  }

  List<Map<String, Object?>> _mergeDiscoveryResults(
      List<Map<String, Object?>> values) {
    final merged = <String, Map<String, Object?>>{};
    for (final item in values) {
      final baseUrl = item['base_url'] as String?;
      if (baseUrl == null || baseUrl.isEmpty) continue;
      merged[baseUrl] = item;
    }
    return merged.values.toList()
      ..sort((left, right) =>
          '${left['base_url']}'.compareTo('${right['base_url']}'));
  }
}

class _DiscoveryPanel extends StatelessWidget {
  const _DiscoveryPanel({
    required this.scanning,
    required this.probing,
    required this.status,
    required this.results,
    required this.manualProtocol,
    required this.manualHostController,
    required this.manualPortController,
    required this.onProtocolChanged,
    required this.onScan,
    required this.onSelect,
    required this.onProbe,
  });

  final bool scanning;
  final bool probing;
  final String status;
  final List<Map<String, Object?>> results;
  final String manualProtocol;
  final TextEditingController manualHostController;
  final TextEditingController manualPortController;
  final ValueChanged<String> onProtocolChanged;
  final VoidCallback onScan;
  final ValueChanged<String> onSelect;
  final VoidCallback onProbe;

  @override
  Widget build(BuildContext context) {
    final colors = context.streamColors;
    return LayoutBuilder(
      builder: (context, constraints) {
        final narrow = constraints.maxWidth < 560;
        final fullWidth = math.max(0.0, constraints.maxWidth);
        final protocolWidth = narrow ? fullWidth : 120.0;
        final hostWidth = narrow
            ? fullWidth
            : math
                .min(260.0, math.max(180.0, constraints.maxWidth - 420))
                .toDouble();
        final portWidth = narrow ? fullWidth : 120.0;
        return DecoratedBox(
          decoration: BoxDecoration(
            color: colors.surfaceAlt.withValues(alpha: 0.72),
            border: Border.all(color: colors.border),
            borderRadius: BorderRadius.circular(8),
          ),
          child: Padding(
            padding: const EdgeInsets.all(12),
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Wrap(
                  spacing: 12,
                  runSpacing: 8,
                  alignment: WrapAlignment.spaceBetween,
                  crossAxisAlignment: WrapCrossAlignment.center,
                  children: [
                    SizedBox(
                      width: 220,
                      child: Text(
                        '服务器发现',
                        style: TextStyle(
                          color: colors.textPrimary,
                          fontWeight: FontWeight.w800,
                        ),
                      ),
                    ),
                    OutlinedButton.icon(
                      onPressed: scanning ? null : onScan,
                      icon: scanning
                          ? const SizedBox.square(
                              dimension: 16,
                              child: CircularProgressIndicator(strokeWidth: 2),
                            )
                          : const Icon(LucideIcons.radar),
                      label: Text(scanning ? '扫描中' : '扫描局域网'),
                    ),
                  ],
                ),
                const SizedBox(height: 12),
                Wrap(
                  spacing: 8,
                  runSpacing: 8,
                  crossAxisAlignment: WrapCrossAlignment.center,
                  children: [
                    SizedBox(
                      width: protocolWidth,
                      child: DropdownButtonFormField<String>(
                        initialValue: manualProtocol,
                        decoration: const InputDecoration(labelText: '协议'),
                        items: const [
                          DropdownMenuItem(value: 'http', child: Text('http')),
                          DropdownMenuItem(
                              value: 'https', child: Text('https')),
                        ],
                        onChanged: scanning || probing
                            ? null
                            : (value) {
                                if (value != null) onProtocolChanged(value);
                              },
                      ),
                    ),
                    SizedBox(
                      width: hostWidth,
                      child: TextField(
                        controller: manualHostController,
                        enabled: !scanning && !probing,
                        decoration: const InputDecoration(labelText: 'IP / 域名'),
                      ),
                    ),
                    SizedBox(
                      width: portWidth,
                      child: TextField(
                        controller: manualPortController,
                        enabled: !scanning && !probing,
                        decoration: const InputDecoration(labelText: '端口'),
                      ),
                    ),
                    FilledButton.icon(
                      onPressed: scanning || probing ? null : onProbe,
                      icon: probing
                          ? const SizedBox.square(
                              dimension: 16,
                              child: CircularProgressIndicator(strokeWidth: 2),
                            )
                          : const Icon(LucideIcons.searchCheck),
                      label: Text(probing ? '探测中' : '探测'),
                    ),
                  ],
                ),
                if (scanning || status.isNotEmpty) ...[
                  const SizedBox(height: 12),
                  if (scanning) const LinearProgressIndicator(minHeight: 3),
                  if (status.isNotEmpty) ...[
                    const SizedBox(height: 8),
                    Text(
                      status,
                      style: TextStyle(
                        color: colors.textSecondary,
                        fontSize: 12,
                      ),
                    ),
                  ],
                ],
                if (results.isNotEmpty) ...[
                  const SizedBox(height: 12),
                  ConstrainedBox(
                    constraints: const BoxConstraints(maxHeight: 150),
                    child: SingleChildScrollView(
                      child: Wrap(
                        spacing: 8,
                        runSpacing: 8,
                        children: results.map((row) {
                          final baseUrl = '${row['base_url']}';
                          return ActionChip(
                            avatar: const Icon(LucideIcons.server, size: 18),
                            label: Text(
                                '$baseUrl · ${row['latency_ms'] ?? '-'}ms'),
                            onPressed: () => onSelect(baseUrl),
                          );
                        }).toList(),
                      ),
                    ),
                  ),
                ],
              ],
            ),
          ),
        );
      },
    );
  }
}
