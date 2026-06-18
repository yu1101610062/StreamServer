import 'package:flutter/material.dart';
import 'package:lucide_icons_flutter/lucide_icons.dart';

import '../core/widgets/stream_data_grid.dart';
import '../state.dart';
import '../utils.dart';
import '../widgets/data_panel.dart';
import 'screen_helpers.dart';

class NodesScreen extends StatefulWidget {
  const NodesScreen({super.key});

  @override
  State<NodesScreen> createState() => _NodesScreenState();
}

class _NodesScreenState extends State<NodesScreen> {
  final keywordController = TextEditingController();
  String healthFilter = '';
  int refreshSeed = 0;
  String output = '';

  @override
  void dispose() {
    keywordController.dispose();
    super.dispose();
  }

  void _refresh() => setState(() => refreshSeed++);

  @override
  Widget build(BuildContext context) {
    final controller = AppScope.of(context);
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        const PageHeader(
          title: '节点中心',
          description: '查看 Agent/ZLM/FFmpeg 健康状态、能力、容量和最近心跳。',
        ),
        Surface(
          child: FilterBar(
            onApply: _refresh,
            onReset: () {
              keywordController.clear();
              healthFilter = '';
              _refresh();
            },
            children: [
              SmallTextField(
                  controller: keywordController,
                  label: '节点关键字',
                  onSubmitted: (_) => _refresh()),
              SmallSelect(
                label: '健康',
                value: healthFilter,
                options: const ['', 'healthy', 'unhealthy'],
                onChanged: (value) => setState(() => healthFilter = value),
              ),
            ],
          ),
        ),
        if (output.isNotEmpty) ...[
          const SizedBox(height: 12),
          Surface(child: SelectableText(output)),
        ],
        const SizedBox(height: 12),
        AsyncDataPanel(
          key: ValueKey(refreshSeed),
          loader: (controller) => controller.api('GET', '/api/v1/nodes'),
          builder: (context, data) {
            final rows = _filterRows(rowsFrom((data as Map)['value']));
            return Surface(
              child: LayoutBuilder(
                builder: (context, constraints) {
                  if (constraints.maxWidth < 820) {
                    return _CompactNodesList(
                      rows: rows,
                      onHeartbeats: (row) => _debug(
                          controller, '/api/v1/nodes/${row['id']}/heartbeats'),
                    );
                  }
                  return StreamDataGrid(
                    height: 560,
                    rows: rows,
                    columns: [
                      StreamGridColumn(
                        title: '节点',
                        field: 'node_name',
                        width: 230,
                        renderer: (context, row, value) => gridTextCell(
                          context,
                          row['node_name'] ?? row['id'],
                          fontWeight: FontWeight.w800,
                          maxWidth: 220,
                        ),
                      ),
                      StreamGridColumn(
                        title: '健康',
                        field: 'healthy',
                        width: 120,
                        renderer: (context, row, value) => StatusBadge(
                          status:
                              row['healthy'] == true ? 'healthy' : 'unhealthy',
                        ),
                      ),
                      StreamGridColumn(
                        title: '控制连接',
                        field: 'control_connected',
                        width: 130,
                        renderer: (context, row, value) => StatusBadge(
                          status: row['control_connected'] == true
                              ? 'connected'
                              : 'disconnected',
                        ),
                      ),
                      StreamGridColumn(
                        title: '媒体',
                        field: 'media_alive',
                        width: 100,
                        renderer: (context, row, value) => StatusBadge(
                          status: row['media_alive'] == true ? 'alive' : 'dead',
                        ),
                      ),
                      const StreamGridColumn(
                        title: 'CPU',
                        field: 'cpu_percent',
                        width: 90,
                      ),
                      const StreamGridColumn(
                        title: '内存',
                        field: 'mem_percent',
                        width: 90,
                      ),
                      const StreamGridColumn(
                        title: '任务',
                        field: 'running_tasks',
                        width: 90,
                      ),
                      StreamGridColumn(
                        title: '槽位',
                        field: 'runtime_slot_loads',
                        width: 240,
                        renderer: (context, row, value) => gridTextCell(
                            context, runtimeSlotLoadsLabel(value),
                            maxWidth: 230),
                      ),
                      StreamGridColumn(
                        title: '标签',
                        field: 'labels',
                        width: 260,
                        renderer: (context, row, value) =>
                            gridTextCell(context, value, maxWidth: 250),
                      ),
                      StreamGridColumn(
                        title: '心跳',
                        field: 'id',
                        width: 120,
                        renderer: (context, row, value) => TextButton.icon(
                          onPressed: () => _debug(controller,
                              '/api/v1/nodes/${row['id']}/heartbeats'),
                          icon: const Icon(LucideIcons.clock, size: 16),
                          label: const Text('查看'),
                        ),
                      ),
                    ],
                  );
                },
              ),
            );
          },
        ),
      ],
    );
  }

  List<Map<String, Object?>> _filterRows(List<Map<String, Object?>> rows) {
    final keyword = keywordController.text.trim().toLowerCase();
    return rows.where((row) {
      final text =
          '${row['id']} ${row['node_name']} ${row['labels']}'.toLowerCase();
      if (keyword.isNotEmpty && !text.contains(keyword)) return false;
      if (healthFilter == 'healthy' && row['healthy'] != true) return false;
      if (healthFilter == 'unhealthy' && row['healthy'] == true) return false;
      return true;
    }).toList();
  }

  Future<void> _debug(AppController controller, String path) async {
    try {
      final result = await controller.api('GET', path);
      setState(() => output = prettyJson(result));
    } catch (cause) {
      setState(() => output = cause.toString());
    }
  }
}

class _CompactNodesList extends StatelessWidget {
  const _CompactNodesList({required this.rows, required this.onHeartbeats});

  final List<Map<String, Object?>> rows;
  final ValueChanged<Map<String, Object?>> onHeartbeats;

  @override
  Widget build(BuildContext context) {
    if (rows.isEmpty) {
      return const SizedBox(
        height: 110,
        child: Center(child: Text('暂无节点')),
      );
    }
    return Column(
      children: [
        for (var index = 0; index < rows.length; index++) ...[
          _CompactNodeItem(row: rows[index], onHeartbeats: onHeartbeats),
          if (index != rows.length - 1)
            const Divider(height: 24, color: Color(0xffe4e8f0)),
        ],
      ],
    );
  }
}

class _CompactNodeItem extends StatelessWidget {
  const _CompactNodeItem({required this.row, required this.onHeartbeats});

  final Map<String, Object?> row;
  final ValueChanged<Map<String, Object?>> onHeartbeats;

  @override
  Widget build(BuildContext context) {
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        Row(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Expanded(
              child: Text(
                textValue(row['node_name'] ?? row['id']),
                softWrap: true,
                style: const TextStyle(fontWeight: FontWeight.w700),
              ),
            ),
            const SizedBox(width: 10),
            StatusBadge(
                status: row['healthy'] == true ? 'healthy' : 'unhealthy'),
          ],
        ),
        const SizedBox(height: 10),
        Wrap(
          spacing: 8,
          runSpacing: 8,
          children: [
            StatusBadge(
                status: row['control_connected'] == true
                    ? 'connected'
                    : 'disconnected'),
            StatusBadge(status: row['media_alive'] == true ? 'alive' : 'dead'),
          ],
        ),
        const SizedBox(height: 10),
        Wrap(
          spacing: 14,
          runSpacing: 8,
          children: [
            _NodeMeta(label: 'CPU', value: row['cpu_percent']),
            _NodeMeta(label: '内存', value: row['mem_percent']),
            _NodeMeta(label: '任务', value: row['running_tasks']),
            _NodeMeta(
                label: '槽位',
                value: runtimeSlotLoadsLabel(row['runtime_slot_loads'])),
            _NodeMeta(label: '标签', value: row['labels']),
          ],
        ),
        const SizedBox(height: 8),
        TextButton.icon(
          onPressed: () => onHeartbeats(row),
          icon: const Icon(Icons.history),
          label: const Text('查看心跳'),
        ),
      ],
    );
  }
}

class _NodeMeta extends StatelessWidget {
  const _NodeMeta({required this.label, required this.value});

  final String label;
  final Object? value;

  @override
  Widget build(BuildContext context) {
    return ConstrainedBox(
      constraints: const BoxConstraints(maxWidth: 300),
      child: RichText(
        text: metadataTextSpan(context, label: label, value: value),
        softWrap: true,
      ),
    );
  }
}
