import 'package:flutter/material.dart';

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
              SmallTextField(controller: keywordController, label: '节点关键字', onSubmitted: (_) => _refresh()),
              SmallSelect(
                label: '健康',
                value: healthFilter,
                options: const ['', 'healthy', 'unhealthy'],
                onChanged: (value) => setState(() => healthFilter = value),
              ),
              OutlinedButton.icon(
                onPressed: () => _debug(controller, '/api/v1/debug/zlm/statistic'),
                icon: const Icon(Icons.query_stats),
                label: const Text('ZLM 统计'),
              ),
              OutlinedButton.icon(
                onPressed: () => _debug(controller, '/api/v1/debug/zlm/threads-load'),
                icon: const Icon(Icons.speed),
                label: const Text('线程负载'),
              ),
              OutlinedButton.icon(
                onPressed: () => _debug(controller, '/api/v1/debug/zlm/work-threads-load'),
                icon: const Icon(Icons.memory),
                label: const Text('工作线程'),
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
              child: SingleChildScrollView(
                scrollDirection: Axis.horizontal,
                child: DataTable(
                  columns: const [
                    DataColumn(label: Text('节点')),
                    DataColumn(label: Text('健康')),
                    DataColumn(label: Text('控制连接')),
                    DataColumn(label: Text('媒体')),
                    DataColumn(label: Text('CPU')),
                    DataColumn(label: Text('内存')),
                    DataColumn(label: Text('任务')),
                    DataColumn(label: Text('标签')),
                    DataColumn(label: Text('心跳')),
                  ],
                  rows: rows.map((row) {
                    return DataRow(cells: [
                      DataCell(Text(textValue(row['node_name'] ?? row['id']))),
                      DataCell(Text(textValue(row['healthy']))),
                      DataCell(Text(textValue(row['control_connected']))),
                      DataCell(Text(textValue(row['media_alive']))),
                      DataCell(Text(textValue(row['cpu_percent']))),
                      DataCell(Text(textValue(row['mem_percent']))),
                      DataCell(Text(textValue(row['running_tasks']))),
                      DataCell(Text(textValue(row['labels']))),
                      DataCell(TextButton.icon(
                        onPressed: () => _debug(controller, '/api/v1/nodes/${row['id']}/heartbeats'),
                        icon: const Icon(Icons.history),
                        label: const Text('查看'),
                      )),
                    ]);
                  }).toList(),
                ),
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
      final text = '${row['id']} ${row['node_name']} ${row['labels']}'.toLowerCase();
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
