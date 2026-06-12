import 'dart:math' as math;

import 'package:flutter/material.dart';

import '../core/theme/stream_theme.dart';
import '../state.dart';
import '../utils.dart';
import '../widgets/data_panel.dart';
import 'screen_helpers.dart';

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
            final events = await controller.api(
                'GET', '/api/v1/tasks/$taskId/events',
                query: {'page_size': 100});
            final logs =
                await controller.api('GET', '/api/v1/tasks/$taskId/logs');
            final streams = await controller
                .api('GET', '/api/v1/streams', query: {'task_id': taskId});
            return {
              'detail': detail,
              'events': events,
              'logs': logs,
              'streams': streams
            };
          },
          builder: (context, data) {
            final map = (data as Map).cast<String, Object?>();
            final detail = (map['detail'] as Map).cast<String, Object?>();
            final task = (detail['task'] as Map?)?.cast<String, Object?>() ??
                <String, Object?>{};
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
                _TaskOperations(
                    taskId: taskId,
                    detail: detail,
                    onDone: () => setState(() => refreshSeed++)),
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
                              _KeepAliveTab(child: _Overview(detail)),
                              _KeepAliveTab(child: _EventsTable(events)),
                              _KeepAliveTab(child: _LogsTable(logs)),
                              _KeepAliveTab(child: _StreamsTable(streams)),
                              _KeepAliveTab(child: _RecordsTable(records)),
                              _KeepAliveTab(child: _ArtifactsTable(artifacts)),
                              _KeepAliveTab(
                                child: SingleChildScrollView(
                                  child: SelectableText(prettyJson(detail)),
                                ),
                              ),
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
  const _TaskOperations(
      {required this.taskId, required this.detail, required this.onDone});

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
          FilledButton.icon(
              onPressed: () => _mutate(context, controller, '启动任务', 'POST',
                  '/api/v1/tasks/$taskId/start'),
              icon: const Icon(Icons.play_arrow),
              label: const Text('启动')),
          OutlinedButton.icon(
              onPressed: () => _confirmMutate(context, controller, '停止任务',
                  'POST', '/api/v1/tasks/$taskId/stop'),
              icon: const Icon(Icons.stop),
              label: const Text('停止')),
          OutlinedButton.icon(
              onPressed: () => _confirmMutate(context, controller, '取消任务',
                  'POST', '/api/v1/tasks/$taskId/cancel'),
              icon: const Icon(Icons.cancel),
              label: const Text('取消')),
          OutlinedButton.icon(
              onPressed: () => _mutate(context, controller, '重试任务', 'POST',
                  '/api/v1/tasks/$taskId/retry'),
              icon: const Icon(Icons.replay),
              label: const Text('重试')),
          OutlinedButton.icon(
              onPressed: () => _mutate(context, controller, '克隆任务', 'POST',
                  '/api/v1/tasks/$taskId/clone',
                  body: detail['requested_spec'] ?? {}),
              icon: const Icon(Icons.copy),
              label: const Text('克隆')),
          FilledButton.tonalIcon(
              onPressed: () => _mutate(context, controller, '开始录制', 'POST',
                  '/api/v1/tasks/$taskId/recording/start',
                  body: {'format': 'mp4'}),
              icon: const Icon(Icons.fiber_manual_record),
              label: const Text('开始录制')),
          OutlinedButton.icon(
              onPressed: () => _confirmMutate(context, controller, '停止录制',
                  'POST', '/api/v1/tasks/$taskId/recording/stop',
                  body: {'reason': 'desktop_user_requested'}),
              icon: const Icon(Icons.stop_circle),
              label: const Text('停止录制')),
          TextButton.icon(
              onPressed: () => _confirmMutate(context, controller, '删除任务',
                  'DELETE', '/api/v1/tasks/$taskId'),
              icon: const Icon(Icons.delete),
              label: const Text('删除')),
        ],
      ),
    );
  }

  Future<void> _confirmMutate(BuildContext context, AppController controller,
      String title, String method, String path,
      {Object? body}) async {
    final confirmed = await showDialog<bool>(
      context: context,
      builder: (context) => AlertDialog(
        title: Text(title),
        content: const Text('该操作会改变运行态，请确认。'),
        actions: [
          TextButton(
              onPressed: () => Navigator.of(context).pop(false),
              child: const Text('取消')),
          FilledButton(
              onPressed: () => Navigator.of(context).pop(true),
              child: const Text('确认')),
        ],
      ),
    );
    if (confirmed == true && context.mounted) {
      await _mutate(context, controller, title, method, path, body: body);
    }
  }

  Future<void> _mutate(BuildContext context, AppController controller,
      String title, String method, String path,
      {Object? body}) async {
    try {
      await controller.mutate(method, path, body: body);
      onDone();
      if (context.mounted) {
        showResult(
          context,
          '$title 已提交',
          tone: InlineStatusTone.success,
        );
      }
    } catch (error) {
      if (context.mounted) {
        showResult(
          context,
          error.toString(),
          tone: InlineStatusTone.danger,
        );
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
          SelectableText(
              '当前 Attempt\n${prettyJson(detail['current_attempt'])}'),
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
      rows: rows
          .map((row) => [
                row['created_at'],
                row['source'],
                row['event_level'],
                row['event_type'],
                row['payload']
              ])
          .toList(),
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
      rows: rows.map((row) {
        final urls = (row['play_urls'] as List?)
                ?.map((url) => '$url')
                .where((url) => url.trim().isNotEmpty)
                .toList() ??
            const <String>[];
        final title = '${row['app']}/${row['stream']}';
        return [
          row['schema'],
          title,
          row['viewer_count'],
          PlayableUrlList(urls: urls, title: title, maxVisibleItems: 2),
        ];
      }).toList(),
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
      rows: rows.map((row) {
        final url = '${row['http_url'] ?? ''}';
        return [
          '${row['app']}/${row['stream']}',
          row['file_path'],
          bytesLabel(row['file_size']),
          url.isEmpty
              ? const Text('—')
              : PlayableUrlTile(
                  url: url,
                  title: '录像播放',
                ),
        ];
      }).toList(),
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
      rows: rows.map((row) {
        final url = '${row['http_url'] ?? ''}';
        return [
          row['artifact_kind'],
          row['file_name'],
          row['file_path'],
          url.isEmpty
              ? const Text('—')
              : PlayableUrlTile(
                  url: url,
                  title: '文件产物播放',
                ),
        ];
      }).toList(),
    );
  }
}

class _SimpleTable extends StatelessWidget {
  const _SimpleTable({required this.columns, required this.rows});

  static const double _tablePadding = 12;
  static const double _horizontalMargin = 16;
  static const double _columnSpacing = 20;

  final List<String> columns;
  final List<List<Object?>> rows;

  @override
  Widget build(BuildContext context) {
    if (rows.isEmpty) {
      return const Center(child: Text('暂无数据'));
    }
    return LayoutBuilder(
      builder: (context, constraints) {
        if (constraints.maxWidth < 760) {
          return _CompactSimpleTable(columns: columns, rows: rows);
        }
        final contentWidth = math.max(
          320.0,
          constraints.maxWidth -
              _tablePadding * 2 -
              _horizontalMargin * 2 -
              _columnSpacing * math.max(0, columns.length - 1),
        );
        final widths = _columnWidths(columns, contentWidth);
        final tableWidth =
            widths.fold<double>(0, (total, width) => total + width) +
                _horizontalMargin * 2 +
                _columnSpacing * math.max(0, columns.length - 1);
        return SingleChildScrollView(
          padding: const EdgeInsets.all(_tablePadding),
          scrollDirection: Axis.horizontal,
          child: ConstrainedBox(
            constraints: BoxConstraints(minWidth: tableWidth),
            child: DataTable(
              horizontalMargin: _horizontalMargin,
              columnSpacing: _columnSpacing,
              dataRowMinHeight: 52,
              dataRowMaxHeight: 280,
              columns: columns.asMap().entries.map((entry) {
                return DataColumn(
                  label: SizedBox(
                    width: widths[entry.key],
                    child: Text(entry.value),
                  ),
                );
              }).toList(),
              rows: rows.map((row) {
                return DataRow(
                  cells: row.asMap().entries.map((entry) {
                    final column = columns[entry.key];
                    final value = entry.value;
                    final cellWidth = widths[entry.key];
                    final Widget child;
                    if (column.contains('状态') ||
                        column.toLowerCase().contains('status')) {
                      child = StatusBadge(status: value);
                    } else if (value is Widget) {
                      child = value;
                    } else {
                      child = WrappedTextCell(
                        value: value,
                        maxWidth: cellWidth,
                        selectable: true,
                      );
                    }
                    return DataCell(
                      SizedBox(width: cellWidth, child: child),
                    );
                  }).toList(),
                );
              }).toList(),
            ),
          ),
        );
      },
    );
  }

  List<double> _columnWidths(List<String> columns, double contentWidth) {
    final minimums = columns.map(_minimumColumnWidth).toList();
    final minimumTotal =
        minimums.fold<double>(0, (total, width) => total + width);
    if (minimumTotal >= contentWidth) {
      return minimums;
    }
    final flexes = columns.map(_columnFlex).toList();
    final flexTotal = flexes.fold<double>(0, (total, flex) => total + flex);
    final extra = contentWidth - minimumTotal;
    return [
      for (var index = 0; index < columns.length; index++)
        minimums[index] + extra * (flexes[index] / flexTotal),
    ];
  }

  double _minimumColumnWidth(String column) {
    if (_isUrlColumn(column)) return 520;
    if (column.contains('Payload') || column.contains('日志')) return 420;
    if (column.contains('路径')) return 340;
    if (column.contains('App/Stream') || column.contains('文件')) return 220;
    if (column.contains('时间')) return 190;
    if (column.contains('类型') || column.contains('来源')) return 150;
    if (column.contains('流')) return 220;
    if (column.contains('观众') || column.contains('大小')) return 100;
    return 140;
  }

  double _columnFlex(String column) {
    if (_isUrlColumn(column)) return 3.4;
    if (column.contains('Payload') || column.contains('日志')) return 2.6;
    if (column.contains('路径')) return 2;
    if (column.contains('App/Stream') || column.contains('文件')) return 1.4;
    return 0.8;
  }

  bool _isUrlColumn(String column) {
    return column.contains('播放地址') ||
        column.toUpperCase() == 'HTTP' ||
        column.toUpperCase().contains('URL');
  }
}

class _KeepAliveTab extends StatefulWidget {
  const _KeepAliveTab({required this.child});

  final Widget child;

  @override
  State<_KeepAliveTab> createState() => _KeepAliveTabState();
}

class _KeepAliveTabState extends State<_KeepAliveTab>
    with AutomaticKeepAliveClientMixin {
  @override
  bool get wantKeepAlive => true;

  @override
  Widget build(BuildContext context) {
    super.build(context);
    return widget.child;
  }
}

class _CompactSimpleTable extends StatelessWidget {
  const _CompactSimpleTable({required this.columns, required this.rows});

  final List<String> columns;
  final List<List<Object?>> rows;

  @override
  Widget build(BuildContext context) {
    return Padding(
      padding: const EdgeInsets.all(12),
      child: Column(
        children: [
          for (var rowIndex = 0; rowIndex < rows.length; rowIndex++) ...[
            _CompactSimpleRow(columns: columns, row: rows[rowIndex]),
            if (rowIndex != rows.length - 1)
              const Divider(height: 24, color: Color(0xffe4e8f0)),
          ],
        ],
      ),
    );
  }
}

class _CompactSimpleRow extends StatelessWidget {
  const _CompactSimpleRow({required this.columns, required this.row});

  final List<String> columns;
  final List<Object?> row;

  @override
  Widget build(BuildContext context) {
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        for (var index = 0; index < row.length; index++) ...[
          _CompactSimpleCell(label: columns[index], value: row[index]),
          if (index != row.length - 1) const SizedBox(height: 8),
        ],
      ],
    );
  }
}

class _CompactSimpleCell extends StatelessWidget {
  const _CompactSimpleCell({required this.label, required this.value});

  final String label;
  final Object? value;

  @override
  Widget build(BuildContext context) {
    final colors = context.streamColors;
    final isStatus =
        label.contains('状态') || label.toLowerCase().contains('status');
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        Text(
          label,
          style: TextStyle(
            color: colors.textSecondary,
            fontSize: 12,
            fontWeight: FontWeight.w600,
          ),
        ),
        const SizedBox(height: 3),
        if (isStatus)
          StatusBadge(status: value)
        else if (value is Widget)
          value as Widget
        else
          SelectableText(
            textValue(value),
            style: TextStyle(color: colors.textPrimary, fontSize: 13),
          ),
      ],
    );
  }
}
