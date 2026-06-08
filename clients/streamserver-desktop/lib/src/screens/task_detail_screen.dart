import 'package:flutter/material.dart';

import '../state.dart';
import '../utils.dart';
import '../widgets/data_panel.dart';

class TaskDetailScreen extends StatefulWidget {
  const TaskDetailScreen({super.key});

  @override
  State<TaskDetailScreen> createState() => _TaskDetailScreenState();
}

class _TaskDetailScreenState extends State<TaskDetailScreen> {
  int refreshSeed = 0;

  @override
  Widget build(BuildContext context) {
    final controller = AppScope.of(context);
    final taskId = controller.selectedTaskId;
    if (taskId == null) {
      return const Surface(child: Text('未选择任务'));
    }
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        PageHeader(
          title: '任务详情',
          description: taskId,
          actions: Wrap(
            spacing: 8,
            runSpacing: 8,
            children: [
              OutlinedButton.icon(
                onPressed: () => controller.navigate(AppSection.tasks),
                icon: const Icon(Icons.arrow_back),
                label: const Text('返回'),
              ),
              IconButton(
                tooltip: '刷新',
                onPressed: () => setState(() => refreshSeed++),
                icon: const Icon(Icons.refresh),
              ),
            ],
          ),
        ),
        AsyncDataPanel(
          key: ValueKey('$taskId-$refreshSeed'),
          loader: (controller) async {
            final detail = await controller.api('GET', '/api/v1/tasks/$taskId');
            final events = await controller.api('GET', '/api/v1/tasks/$taskId/events', query: {'page_size': 100});
            final logs = await controller.api('GET', '/api/v1/tasks/$taskId/logs');
            final streams = await controller.api('GET', '/api/v1/streams', query: {'task_id': taskId});
            return {'detail': detail, 'events': events, 'logs': logs, 'streams': streams};
          },
          builder: (context, data) {
            final map = (data as Map).cast<String, Object?>();
            final detail = (map['detail'] as Map).cast<String, Object?>();
            final task = (detail['task'] as Map?)?.cast<String, Object?>() ?? <String, Object?>{};
            final events = rowsFrom((map['events'] as Map?)?['items']);
            final logs = rowsFrom((map['logs'] as Map?)?['lines']);
            final records = rowsFrom(detail['records']);
            final artifacts = rowsFrom(detail['file_artifacts']);
            final streams = rowsFrom((map['streams'] as Map?)?['value']);
            return Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                KeyValueGrid(
                  items: {
                    '名称': task['name'],
                    '类型': task['type'],
                    '状态': task['status'],
                    '优先级': task['priority'],
                    'Attempt': task['current_attempt_no'],
                    '节点': shortId(task['assigned_node_id']),
                  },
                ),
                const SizedBox(height: 12),
                _TaskOperations(taskId: taskId, detail: detail, onDone: () => setState(() => refreshSeed++)),
                const SizedBox(height: 12),
                DefaultTabController(
                  length: 7,
                  child: Surface(
                    child: Column(
                      crossAxisAlignment: CrossAxisAlignment.start,
                      children: [
                        const TabBar(
                          isScrollable: true,
                          tabs: [
                            Tab(text: '概览'),
                            Tab(text: '事件'),
                            Tab(text: '日志'),
                            Tab(text: '在线流'),
                            Tab(text: '录像'),
                            Tab(text: '文件产物'),
                            Tab(text: '规格 JSON'),
                          ],
                        ),
                        SizedBox(
                          height: 560,
                          child: TabBarView(
                            children: [
                              _Overview(detail),
                              _EventsTable(events),
                              _LogsTable(logs),
                              _StreamsTable(streams),
                              _RecordsTable(records),
                              _ArtifactsTable(artifacts),
                              SingleChildScrollView(child: SelectableText(prettyJson(detail))),
                            ],
                          ),
                        ),
                      ],
                    ),
                  ),
                ),
              ],
            );
          },
        ),
      ],
    );
  }
}

class _TaskOperations extends StatelessWidget {
  const _TaskOperations({required this.taskId, required this.detail, required this.onDone});

  final String taskId;
  final Map<String, Object?> detail;
  final VoidCallback onDone;

  @override
  Widget build(BuildContext context) {
    final controller = AppScope.of(context);
    return Surface(
      child: Wrap(
        spacing: 8,
        runSpacing: 8,
        children: [
          FilledButton.icon(onPressed: () => _mutate(context, controller, '启动任务', 'POST', '/api/v1/tasks/$taskId/start'), icon: const Icon(Icons.play_arrow), label: const Text('启动')),
          OutlinedButton.icon(onPressed: () => _confirmMutate(context, controller, '停止任务', 'POST', '/api/v1/tasks/$taskId/stop'), icon: const Icon(Icons.stop), label: const Text('停止')),
          OutlinedButton.icon(onPressed: () => _confirmMutate(context, controller, '取消任务', 'POST', '/api/v1/tasks/$taskId/cancel'), icon: const Icon(Icons.cancel), label: const Text('取消')),
          OutlinedButton.icon(onPressed: () => _mutate(context, controller, '重试任务', 'POST', '/api/v1/tasks/$taskId/retry'), icon: const Icon(Icons.replay), label: const Text('重试')),
          OutlinedButton.icon(onPressed: () => _mutate(context, controller, '克隆任务', 'POST', '/api/v1/tasks/$taskId/clone', body: detail['requested_spec'] ?? {}), icon: const Icon(Icons.copy), label: const Text('克隆')),
          FilledButton.tonalIcon(onPressed: () => _mutate(context, controller, '开始录制', 'POST', '/api/v1/tasks/$taskId/recording/start', body: {'format': 'mp4'}), icon: const Icon(Icons.fiber_manual_record), label: const Text('开始录制')),
          OutlinedButton.icon(onPressed: () => _confirmMutate(context, controller, '停止录制', 'POST', '/api/v1/tasks/$taskId/recording/stop', body: {'reason': 'desktop_user_requested'}), icon: const Icon(Icons.stop_circle), label: const Text('停止录制')),
          TextButton.icon(onPressed: () => _confirmMutate(context, controller, '删除任务', 'DELETE', '/api/v1/tasks/$taskId'), icon: const Icon(Icons.delete), label: const Text('删除')),
        ],
      ),
    );
  }

  Future<void> _confirmMutate(BuildContext context, AppController controller, String title, String method, String path, {Object? body}) async {
    final confirmed = await showDialog<bool>(
      context: context,
      builder: (context) => AlertDialog(
        title: Text(title),
        content: const Text('该操作会改变运行态，请确认。'),
        actions: [
          TextButton(onPressed: () => Navigator.of(context).pop(false), child: const Text('取消')),
          FilledButton(onPressed: () => Navigator.of(context).pop(true), child: const Text('确认')),
        ],
      ),
    );
    if (confirmed == true && context.mounted) {
      await _mutate(context, controller, title, method, path, body: body);
    }
  }

  Future<void> _mutate(BuildContext context, AppController controller, String title, String method, String path, {Object? body}) async {
    try {
      await controller.mutate(method, path, body: body);
      onDone();
      if (context.mounted) {
        ScaffoldMessenger.of(context).showSnackBar(SnackBar(content: Text('$title 已提交')));
      }
    } catch (error) {
      if (context.mounted) {
        ScaffoldMessenger.of(context).showSnackBar(SnackBar(content: Text(error.toString())));
      }
    }
  }
}

class _Overview extends StatelessWidget {
  const _Overview(this.detail);

  final Map<String, Object?> detail;

  @override
  Widget build(BuildContext context) {
    return SingleChildScrollView(
      padding: const EdgeInsets.all(12),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          SelectableText('当前 Attempt\n${prettyJson(detail['current_attempt'])}'),
          const SizedBox(height: 16),
          SelectableText('回调状态\n${prettyJson(detail['callback_delivery'])}'),
        ],
      ),
    );
  }
}

class _EventsTable extends StatelessWidget {
  const _EventsTable(this.rows);

  final List<Map<String, Object?>> rows;

  @override
  Widget build(BuildContext context) {
    return _SimpleTable(
      columns: const ['时间', '来源', '等级', '类型', 'Payload'],
      rows: rows.map((row) => [row['created_at'], row['source'], row['event_level'], row['event_type'], row['payload']]).toList(),
    );
  }
}

class _LogsTable extends StatelessWidget {
  const _LogsTable(this.rows);

  final List<Map<String, Object?>> rows;

  @override
  Widget build(BuildContext context) {
    return _SimpleTable(
      columns: const ['时间', '流', '日志'],
      rows: rows.map((row) => [row['ts'], row['stream'], row['line']]).toList(),
    );
  }
}

class _StreamsTable extends StatelessWidget {
  const _StreamsTable(this.rows);

  final List<Map<String, Object?>> rows;

  @override
  Widget build(BuildContext context) {
    return _SimpleTable(
      columns: const ['协议', 'App/Stream', '观众', '播放地址'],
      rows: rows.map((row) => [row['schema'], '${row['app']}/${row['stream']}', row['viewer_count'], row['play_urls']]).toList(),
    );
  }
}

class _RecordsTable extends StatelessWidget {
  const _RecordsTable(this.rows);

  final List<Map<String, Object?>> rows;

  @override
  Widget build(BuildContext context) {
    return _SimpleTable(
      columns: const ['流', '路径', '大小', 'HTTP'],
      rows: rows.map((row) => ['${row['app']}/${row['stream']}', row['file_path'], bytesLabel(row['file_size']), row['http_url']]).toList(),
    );
  }
}

class _ArtifactsTable extends StatelessWidget {
  const _ArtifactsTable(this.rows);

  final List<Map<String, Object?>> rows;

  @override
  Widget build(BuildContext context) {
    return _SimpleTable(
      columns: const ['类型', '文件', '路径', 'HTTP'],
      rows: rows.map((row) => [row['artifact_kind'], row['file_name'], row['file_path'], row['http_url']]).toList(),
    );
  }
}

class _SimpleTable extends StatelessWidget {
  const _SimpleTable({required this.columns, required this.rows});

  final List<String> columns;
  final List<List<Object?>> rows;

  @override
  Widget build(BuildContext context) {
    if (rows.isEmpty) {
      return const Center(child: Text('暂无数据'));
    }
    return SingleChildScrollView(
      padding: const EdgeInsets.all(12),
      scrollDirection: Axis.horizontal,
      child: DataTable(
        columns: columns.map((column) => DataColumn(label: Text(column))).toList(),
        rows: rows.map((row) {
          return DataRow(cells: row.map((value) => DataCell(SelectableText(textValue(value)))).toList());
        }).toList(),
      ),
    );
  }
}
