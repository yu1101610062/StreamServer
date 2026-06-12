import 'package:flutter/material.dart';

import '../core/theme/stream_theme.dart';
import '../state.dart';
import '../utils.dart';
import '../widgets/data_panel.dart';
import 'screen_helpers.dart';

class DebugScreen extends StatefulWidget {
  const DebugScreen({super.key});

  @override
  State<DebugScreen> createState() => _DebugScreenState();
}

class _DebugScreenState extends State<DebugScreen> {
  final nodeController = TextEditingController();
  final schemaController = TextEditingController();
  final vhostController = TextEditingController(text: '__defaultVhost__');
  final appController = TextEditingController();
  final streamController = TextEditingController();
  final sessionController = TextEditingController();
  final localPortController = TextEditingController();
  final peerIpController = TextEditingController();
  final snapUrlController = TextEditingController();
  final timeoutController = TextEditingController(text: '10');
  final playerSessionController = TextEditingController();
  final snapshotPathController = TextEditingController();
  String output = '';
  bool running = false;

  @override
  void dispose() {
    nodeController.dispose();
    schemaController.dispose();
    vhostController.dispose();
    appController.dispose();
    streamController.dispose();
    sessionController.dispose();
    localPortController.dispose();
    peerIpController.dispose();
    snapUrlController.dispose();
    timeoutController.dispose();
    playerSessionController.dispose();
    snapshotPathController.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final controller = AppScope.of(context);
    return Theme(
      data: StreamTheme.dark(),
      child: Builder(builder: (context) {
        return Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            const PageHeader(
              title: '调试台',
              description: '运行 Core 健康探测、播放器后端探测和 ZLM 常用调试接口。',
            ),
            Surface(
              child: Wrap(
                spacing: 8,
                runSpacing: 8,
                children: [
                  FilledButton.icon(
                    onPressed: running
                        ? null
                        : () => _run(() => controller.diagnostics()),
                    icon: const Icon(Icons.health_and_safety),
                    label: const Text('Core 探测'),
                  ),
                  OutlinedButton.icon(
                    onPressed:
                        running ? null : () => _nativePlayerProbe(controller),
                    icon: const Icon(Icons.video_settings),
                    label: const Text('播放器探测'),
                  ),
                  _debugButton(controller, '/api/v1/debug/zlm/media',
                      Icons.live_tv, 'ZLM 媒体'),
                  _debugButton(controller, '/api/v1/debug/zlm/sessions',
                      Icons.people, '会话'),
                  _debugButton(controller, '/api/v1/debug/zlm/players',
                      Icons.smart_display, '播放器'),
                  _debugButton(controller, '/api/v1/debug/zlm/statistic',
                      Icons.query_stats, '统计'),
                  _debugButton(controller, '/api/v1/debug/zlm/threads-load',
                      Icons.speed, '线程负载'),
                  _debugButton(
                      controller,
                      '/api/v1/debug/zlm/work-threads-load',
                      Icons.memory,
                      '工作线程'),
                  _debugButton(controller, '/api/v1/debug/hooks', Icons.webhook,
                      'Hook 事件'),
                ],
              ),
            ),
            const SizedBox(height: 12),
            Surface(
              child: Column(
                crossAxisAlignment: CrossAxisAlignment.start,
                children: [
                  const Text('ZLM 操作',
                      style: TextStyle(fontWeight: FontWeight.w700)),
                  const SizedBox(height: 12),
                  Wrap(
                    spacing: 12,
                    runSpacing: 12,
                    crossAxisAlignment: WrapCrossAlignment.center,
                    children: [
                      SmallTextField(
                          controller: nodeController, label: '节点 ID'),
                      SmallTextField(
                          controller: schemaController,
                          label: '协议',
                          width: 140),
                      SmallTextField(
                          controller: vhostController, label: 'Vhost'),
                      SmallTextField(controller: appController, label: 'App'),
                      SmallTextField(
                          controller: streamController, label: 'Stream'),
                      SmallTextField(
                          controller: sessionController, label: 'Session ID'),
                      FilledButton.icon(
                        onPressed:
                            running ? null : () => _kickSession(controller),
                        icon: const Icon(Icons.person_remove),
                        label: const Text('踢会话'),
                      ),
                      SmallTextField(
                          controller: localPortController,
                          label: '本地端口',
                          width: 160),
                      SmallTextField(
                          controller: peerIpController, label: 'Peer IP'),
                      FilledButton.icon(
                        onPressed:
                            running ? null : () => _kickSessions(controller),
                        icon: const Icon(Icons.group_remove),
                        label: const Text('批量踢会话'),
                      ),
                    ],
                  ),
                ],
              ),
            ),
            const SizedBox(height: 12),
            Surface(
              child: Column(
                crossAxisAlignment: CrossAxisAlignment.start,
                children: [
                  const Text('本地播放器',
                      style: TextStyle(fontWeight: FontWeight.w700)),
                  const SizedBox(height: 12),
                  Wrap(
                    spacing: 12,
                    runSpacing: 12,
                    crossAxisAlignment: WrapCrossAlignment.center,
                    children: [
                      SmallTextField(
                          controller: playerSessionController,
                          label: 'Session ID'),
                      SmallTextField(
                          controller: snapshotPathController,
                          label: '截图输出路径',
                          width: 360),
                      FilledButton.icon(
                        onPressed:
                            running ? null : () => _stopPlayer(controller),
                        icon: const Icon(Icons.stop),
                        label: const Text('停止播放'),
                      ),
                      FilledButton.icon(
                        onPressed:
                            running ? null : () => _snapshotPlayer(controller),
                        icon: const Icon(Icons.camera),
                        label: const Text('本地截图'),
                      ),
                    ],
                  ),
                  const SizedBox(height: 18),
                  const Text('截图探测',
                      style: TextStyle(fontWeight: FontWeight.w700)),
                  const SizedBox(height: 12),
                  Wrap(
                    spacing: 12,
                    runSpacing: 12,
                    crossAxisAlignment: WrapCrossAlignment.center,
                    children: [
                      SmallTextField(
                          controller: snapUrlController,
                          label: '播放 URL',
                          width: 520),
                      SmallTextField(
                          controller: timeoutController,
                          label: '超时秒',
                          width: 120),
                      FilledButton.icon(
                        onPressed: running ? null : () => _snap(controller),
                        icon: const Icon(Icons.photo_camera),
                        label: const Text('截图'),
                      ),
                    ],
                  ),
                ],
              ),
            ),
            if (output.isNotEmpty) ...[
              const SizedBox(height: 12),
              Surface(
                child: SelectableText(
                  output,
                  style: const TextStyle(
                    fontFamily: 'Menlo',
                    fontSize: 12,
                    height: 1.45,
                  ),
                ),
              ),
            ],
          ],
        );
      }),
    );
  }

  Widget _debugButton(
      AppController controller, String path, IconData icon, String label) {
    return OutlinedButton.icon(
      onPressed: running
          ? null
          : () =>
              _run(() => controller.api('GET', path, query: _debugQuery(path))),
      icon: Icon(icon),
      label: Text(label),
    );
  }

  Map<String, Object?> _debugQuery(String path) {
    final query = <String, Object?>{
      'node_id': nodeController.text,
    };
    if (path.endsWith('/players')) {
      query.addAll({
        'schema': schemaController.text,
        'vhost': vhostController.text,
        'app': appController.text,
        'stream': streamController.text,
      });
    }
    return cleanQuery(query);
  }

  Future<void> _nativePlayerProbe(AppController controller) async {
    await _run(() async {
      final bridgeProbe = await controller.openMediaProbe();
      return bridgeProbe;
    });
  }

  Future<void> _kickSession(AppController controller) async {
    final confirmed = await confirmAction(
      context,
      title: '踢出会话',
      message: '确认踢出节点 ${nodeController.text} 的会话 ${sessionController.text}？',
      confirmLabel: '踢出',
      destructive: true,
    );
    if (!confirmed) return;
    await _run(() {
      return controller.api(
        'POST',
        '/api/v1/debug/zlm/kick-session',
        body: {
          'node_id': nodeController.text,
          'session_id': sessionController.text,
        },
      );
    });
  }

  Future<void> _kickSessions(AppController controller) async {
    final confirmed = await confirmAction(
      context,
      title: '批量踢出会话',
      message: '确认按本地端口或 Peer IP 批量踢出节点 ${nodeController.text} 上的会话？',
      confirmLabel: '批量踢出',
      destructive: true,
    );
    if (!confirmed) return;
    await _run(() {
      return controller.api(
        'POST',
        '/api/v1/debug/zlm/kick-sessions',
        body: cleanQuery({
          'node_id': nodeController.text,
          'local_port': int.tryParse(localPortController.text),
          'peer_ip': peerIpController.text,
        }),
      );
    });
  }

  Future<void> _snap(AppController controller) async {
    await _run(() {
      return controller.api(
        'GET',
        '/api/v1/debug/zlm/snap',
        query: cleanQuery({
          'node_id': nodeController.text,
          'url': snapUrlController.text,
          'timeout_sec': int.tryParse(timeoutController.text),
          'expire_sec': 30,
        }),
      );
    });
  }

  Future<void> _stopPlayer(AppController controller) async {
    await _run(() => controller.stopMedia(playerSessionController.text));
  }

  Future<void> _snapshotPlayer(AppController controller) async {
    await _run(() => controller.snapshotMedia(playerSessionController.text,
        outputPath: snapshotPathController.text));
  }

  Future<void> _run(Future<Object?> Function() action) async {
    setState(() {
      running = true;
      output = '';
    });
    try {
      final value = await action();
      setState(() => output = prettyJson(value));
    } catch (cause) {
      setState(() => output = cause.toString());
    } finally {
      if (mounted) setState(() => running = false);
    }
  }
}
