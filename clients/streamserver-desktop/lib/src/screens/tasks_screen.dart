import 'dart:math' as math;

import 'package:flutter/material.dart';

import '../state.dart';
import '../utils.dart';
import '../widgets/data_panel.dart';
import 'screen_helpers.dart';

class TasksScreen extends StatefulWidget {
  const TasksScreen({super.key});

  @override
  State<TasksScreen> createState() => _TasksScreenState();
}

class _TasksScreenState extends State<TasksScreen> {
  final keywordController = TextEditingController();
  final nodeController = TextEditingController();
  String statusFilter = '';
  String typeFilter = '';
  int page = 1;
  final int pageSize = 50;
  int refreshSeed = 0;

  @override
  void dispose() {
    keywordController.dispose();
    nodeController.dispose();
    super.dispose();
  }

  void _refresh({bool resetPage = false}) {
    setState(() {
      if (resetPage) page = 1;
      refreshSeed++;
    });
  }

  @override
  Widget build(BuildContext context) {
    final controller = AppScope.of(context);
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        PageHeader(
          title: '任务中心',
          description: '查看任务状态，执行启动、停止、取消、重试、克隆和删除等控制操作。',
          actions: FilledButton.icon(
            onPressed: () => controller.navigate(AppSection.taskCreate),
            icon: const Icon(Icons.add),
            label: const Text('新建任务'),
          ),
        ),
        Surface(
          child: FilterBar(
            onApply: () => _refresh(resetPage: true),
            onReset: () {
              keywordController.clear();
              nodeController.clear();
              statusFilter = '';
              typeFilter = '';
              _refresh(resetPage: true);
            },
            children: [
              SmallTextField(
                controller: keywordController,
                label: '关键字',
                width: 260,
                onSubmitted: (_) => _refresh(resetPage: true),
              ),
              SmallSelect(
                label: '状态',
                value: statusFilter,
                options: const [
                  '',
                  'CREATED',
                  'VALIDATING',
                  'QUEUED',
                  'STARTING',
                  'RUNNING',
                  'STOPPING',
                  'RECOVERING',
                  'SUCCEEDED',
                  'FAILED',
                  'LOST',
                  'CANCELED',
                ],
                onChanged: (value) => setState(() => statusFilter = value),
              ),
              SmallSelect(
                label: '类型',
                value: typeFilter,
                options: const [
                  '',
                  'stream_ingest',
                  'stream_bridge',
                  'file_transcode'
                ],
                onChanged: (value) => setState(() => typeFilter = value),
                width: 210,
              ),
              SmallTextField(
                controller: nodeController,
                label: '节点 ID',
                width: 220,
                onSubmitted: (_) => _refresh(resetPage: true),
              ),
            ],
          ),
        ),
        const SizedBox(height: 12),
        AsyncDataPanel(
          key: ValueKey(refreshSeed),
          loader: (controller) => controller.api(
            'GET',
            '/api/v1/tasks',
            query: cleanQuery({
              'page': page,
              'page_size': pageSize,
              'keyword': keywordController.text,
              'status': statusFilter,
              'type': typeFilter,
              'node_id': nodeController.text,
              'sort_by': 'created_at',
              'sort_order': 'desc',
            }),
          ),
          builder: (context, data) {
            final payload = (data as Map).cast<String, Object?>();
            final rows = rowsFrom(payload['items']);
            final total = (payload['total'] as num?)?.toInt() ?? rows.length;
            return Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Align(
                  alignment: Alignment.centerRight,
                  child: PagerBar(
                    page: page,
                    pageSize: pageSize,
                    total: total,
                    onPageChanged: (value) {
                      page = value;
                      _refresh();
                    },
                  ),
                ),
                Surface(
                  child: LayoutBuilder(
                    builder: (context, constraints) {
                      if (constraints.maxWidth < 820) {
                        return _CompactTaskList(
                          rows: rows,
                          onDone: _refresh,
                        );
                      }
                      final taskWidth = math.max(
                          180.0, math.min(420.0, constraints.maxWidth * 0.34));
                      final timeWidth = math.max(
                          150.0, math.min(220.0, constraints.maxWidth * 0.18));
                      return SingleChildScrollView(
                        scrollDirection: Axis.horizontal,
                        child: DataTable(
                          dataRowMinHeight: 56,
                          dataRowMaxHeight: 136,
                          columns: const [
                            DataColumn(label: Text('任务')),
                            DataColumn(label: Text('类型')),
                            DataColumn(label: Text('状态')),
                            DataColumn(label: Text('节点')),
                            DataColumn(label: Text('创建者')),
                            DataColumn(label: Text('更新时间')),
                            DataColumn(label: Text('操作')),
                          ],
                          rows: rows.map((row) {
                            return DataRow(
                              cells: [
                                DataCell(
                                  WrappedTextCell(
                                    value: row['name'],
                                    maxWidth: taskWidth,
                                    fontWeight: FontWeight.w600,
                                  ),
                                  onTap: () =>
                                      controller.openTask('${row['id']}'),
                                ),
                                DataCell(WrappedTextCell(
                                    value: row['type'], maxWidth: 150)),
                                DataCell(StatusBadge(status: row['status'])),
                                DataCell(WrappedTextCell(
                                    value: shortId(row['assigned_node_id'] ??
                                        row['node_id']),
                                    maxWidth: 96)),
                                DataCell(WrappedTextCell(
                                    value: row['created_by'], maxWidth: 120)),
                                DataCell(WrappedTextCell(
                                    value: row['updated_at'],
                                    maxWidth: timeWidth)),
                                DataCell(
                                    _TaskActions(row: row, onDone: _refresh)),
                              ],
                            );
                          }).toList(),
                        ),
                      );
                    },
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

class _CompactTaskList extends StatelessWidget {
  const _CompactTaskList({required this.rows, required this.onDone});

  final List<Map<String, Object?>> rows;
  final VoidCallback onDone;

  @override
  Widget build(BuildContext context) {
    if (rows.isEmpty) {
      return const SizedBox(
        height: 120,
        child: Center(child: Text('暂无任务')),
      );
    }
    return Column(
      children: [
        for (var index = 0; index < rows.length; index++) ...[
          _CompactTaskItem(row: rows[index], onDone: onDone),
          if (index != rows.length - 1)
            const Divider(height: 24, color: Color(0xffe4e8f0)),
        ],
      ],
    );
  }
}

class _CompactTaskItem extends StatelessWidget {
  const _CompactTaskItem({required this.row, required this.onDone});

  final Map<String, Object?> row;
  final VoidCallback onDone;

  @override
  Widget build(BuildContext context) {
    final controller = AppScope.of(context);
    final id = '${row['id']}';
    return InkWell(
      borderRadius: BorderRadius.circular(8),
      onTap: () => controller.openTask(id),
      child: Padding(
        padding: const EdgeInsets.symmetric(vertical: 4),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Row(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Expanded(
                  child: Text(
                    textValue(row['name']),
                    softWrap: true,
                    style: const TextStyle(fontWeight: FontWeight.w700),
                  ),
                ),
                const SizedBox(width: 10),
                StatusBadge(status: row['status']),
              ],
            ),
            const SizedBox(height: 10),
            Wrap(
              spacing: 14,
              runSpacing: 8,
              children: [
                _TaskMeta(label: '类型', value: row['type']),
                _TaskMeta(
                  label: '节点',
                  value: shortId(row['assigned_node_id'] ?? row['node_id']),
                ),
                _TaskMeta(label: '创建者', value: row['created_by']),
                _TaskMeta(label: '更新时间', value: row['updated_at']),
              ],
            ),
            const SizedBox(height: 8),
            _TaskActions(row: row, onDone: onDone),
          ],
        ),
      ),
    );
  }
}

class _TaskMeta extends StatelessWidget {
  const _TaskMeta({required this.label, required this.value});

  final String label;
  final Object? value;

  @override
  Widget build(BuildContext context) {
    return ConstrainedBox(
      constraints: const BoxConstraints(maxWidth: 260),
      child: RichText(
        text: TextSpan(
          style: const TextStyle(color: Color(0xff1d2433), fontSize: 13),
          children: [
            TextSpan(
              text: '$label：',
              style: const TextStyle(
                color: Color(0xff5b6477),
                fontWeight: FontWeight.w600,
              ),
            ),
            TextSpan(text: textValue(value)),
          ],
        ),
        softWrap: true,
      ),
    );
  }
}

class _TaskActions extends StatelessWidget {
  const _TaskActions({required this.row, required this.onDone});

  final Map<String, Object?> row;
  final VoidCallback onDone;

  @override
  Widget build(BuildContext context) {
    final controller = AppScope.of(context);
    final id = '${row['id']}';
    final name = textValue(row['name']);
    return Wrap(
      spacing: 4,
      runSpacing: 4,
      children: [
        TextButton(
            onPressed: () => controller.openTask(id), child: const Text('详情')),
        TextButton(
            onPressed: () =>
                _mutate(context, controller, 'POST', '/api/v1/tasks/$id/start'),
            child: const Text('启动')),
        TextButton(
          onPressed: () => _confirmMutate(
            context,
            controller,
            'POST',
            '/api/v1/tasks/$id/stop',
            '停止任务',
            '确认停止任务 $name？',
          ),
          child: const Text('停止'),
        ),
        TextButton(
          onPressed: () => _confirmMutate(
            context,
            controller,
            'POST',
            '/api/v1/tasks/$id/cancel',
            '取消任务',
            '确认取消任务 $name？',
          ),
          child: const Text('取消'),
        ),
        TextButton(
            onPressed: () =>
                _mutate(context, controller, 'POST', '/api/v1/tasks/$id/retry'),
            child: const Text('重试')),
        TextButton(
            onPressed: () =>
                _mutate(context, controller, 'POST', '/api/v1/tasks/$id/clone'),
            child: const Text('克隆')),
        TextButton(
          onPressed: () => _confirmMutate(
            context,
            controller,
            'DELETE',
            '/api/v1/tasks/$id',
            '删除任务',
            '确认删除任务 $name？该操作不可撤销。',
            destructive: true,
          ),
          child: const Text('删除'),
        ),
      ],
    );
  }

  Future<void> _confirmMutate(
    BuildContext context,
    AppController controller,
    String method,
    String path,
    String title,
    String message, {
    bool destructive = false,
  }) async {
    final confirmed = await confirmAction(
      context,
      title: title,
      message: message,
      destructive: destructive,
      confirmLabel: title.replaceAll('任务', ''),
    );
    if (!confirmed) return;
    if (!context.mounted) return;
    await _mutate(context, controller, method, path);
  }

  Future<void> _mutate(
    BuildContext context,
    AppController controller,
    String method,
    String path,
  ) async {
    try {
      await controller.mutate(method, path);
      if (context.mounted) showResult(context, '操作已提交');
      onDone();
    } catch (cause) {
      if (context.mounted) showResult(context, cause.toString());
    }
  }
}
