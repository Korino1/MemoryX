# MemoryX RAG Integration для нейросетей
#
# Этот модуль показывает, как LLM могут полноценно использовать MemoryX:
# - Векторный поиск + граф связей
# - Контекстуализация ответов
# - Обучение на знаниях из MemoryX
#
# Установка:
# pip install memoryx-rag openai torch

import asyncio
import json
from typing import List, Dict, Any, Optional
from dataclasses import dataclass
from openai import AsyncOpenAI

# ============================================================================
# MemoryX Client (через subprocess для Native API)
# ============================================================================


class MemoryXClient:
    """Клиент для MemoryX Native API через subprocess"""

    def __init__(self, data_dir: str = "./memoryx_data"):
        self.data_dir = data_dir
        self.process = None

    async def connect(self):
        """Подключение к MemoryX"""
        # В реальной реализации это будет gRPC или HTTP вызов
        pass

    async def query(
        self, question: str, ctx_id: Optional[int] = None
    ) -> Dict[str, Any]:
        """Query к MemoryX с полным AnswerPack"""
        # Используем Native API через subprocess
        cmd = [
            "cargo",
            "run",
            "--example",
            "native_api",
            "--features",
            "mcp",
            "--",
            "--query",
            question,
            "--data-dir",
            self.data_dir,
        ]

        if ctx_id:
            cmd.extend(["--ctx", str(ctx_id)])

        # Выполняем и парсим результат
        result = await self._run_command(cmd)
        return json.loads(result)

    async def batch_ingest(self, documents: List[Dict[str, Any]]) -> Dict[str, Any]:
        """Batch загрузка документов в MemoryX"""
        # Конвертируем документы в атомы
        atoms = []
        for doc in documents:
            atom = {
                "payload": doc["content"].encode(),
                "atom_type": doc.get("type", "FACT"),
                "claims": doc.get("claims", []),
                "evidence": doc.get("evidence", []),
            }
            atoms.append(atom)

        # Batch ingest через Native API
        cmd = [
            "cargo",
            "run",
            "--example",
            "native_api",
            "--features",
            "mcp",
            "--",
            "--batch-ingest",
            json.dumps(atoms),
            "--data-dir",
            self.data_dir,
        ]

        result = await self._run_command(cmd)
        return json.loads(result)

    async def graph_walk(self, seed_nodes: List[int], depth: int = 3) -> List[Dict]:
        """Обход графа для получения связанных знаний"""
        cmd = [
            "cargo",
            "run",
            "--example",
            "native_api",
            "--features",
            "mcp",
            "--",
            "--graph-walk",
            json.dumps(seed_nodes),
            "--depth",
            str(depth),
            "--data-dir",
            self.data_dir,
        ]

        result = await self._run_command(cmd)
        return json.loads(result)

    async def _run_command(self, cmd: List[str]) -> str:
        """Выполнение команды и получение результата"""
        import subprocess

        proc = await asyncio.create_subprocess_exec(
            *cmd, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE
        )
        stdout, stderr = await proc.communicate()
        if proc.returncode != 0:
            raise Exception(f"Command failed: {stderr.decode()}")
        return stdout.decode()


# ============================================================================
# RAG Pipeline
# ============================================================================


@dataclass
class RAGContext:
    """Контекст для RAG генерации"""

    query: str
    retrieved_atoms: List[Dict]
    graph_edges: List[Dict]
    confidence: float
    limitations: List[str]


class MemoryXRAG:
    """
    RAG система на базе MemoryX для LLM

    Использует:
    1. MemoryX для retrieval знаний
    2. Graph traversal для связей
    3. LLM для генерации ответов
    """

    def __init__(
        self,
        memoryx_client: MemoryXClient,
        llm_client: AsyncOpenAI,
        model: str = "gpt-4",
    ):
        self.memoryx = memoryx_client
        self.llm = llm_client
        self.model = model

    async def generate_answer(
        self, question: str, max_atoms: int = 10, graph_depth: int = 2
    ) -> str:
        """
        Генерация ответа с использованием MemoryX

        Args:
            question: Вопрос пользователя
            max_atoms: Максимальное количество атомов для retrieval
            graph_depth: Глубина обхода графа

        Returns:
            Ответ от LLM с контекстом из MemoryX
        """
        # Шаг 1: Query к MemoryX
        query_result = await self.memoryx.query(question)

        # Шаг 2: Извлекаем атомы
        atoms = query_result.get("atoms", [])[:max_atoms]

        # Шаг 3: Получаем связанные знания через graph walk
        seed_nodes = [atom["node_num"] for atom in atoms if "node_num" in atom]
        edges = await self.memoryx.graph_walk(seed_nodes, graph_depth)

        # Шаг 4: Формируем контекст
        context = self._build_context(atoms, edges)

        # Шаг 5: Генерируем ответ через LLM
        answer = await self._generate_with_context(question, context)

        return answer

    def _build_context(self, atoms: List[Dict], edges: List[Dict]) -> str:
        """Построение контекста из атомов и графа"""
        context_parts = []

        # Атомы
        context_parts.append("=== Знания из MemoryX ===")
        for i, atom in enumerate(atoms, 1):
            content = atom.get("content", "")
            atom_type = atom.get("type", "FACT")
            confidence = atom.get("confidence", 0.0)

            context_parts.append(
                f"[{i}] ({atom_type}, confidence={confidence:.2f})\n{content}\n"
            )

        # Связи
        if edges:
            context_parts.append("\n=== Связанные понятия ===")
            for edge in edges[:10]:
                src = edge.get("src", "?")
                dst = edge.get("dst", "?")
                edge_type = edge.get("type", "RELATED")
                context_parts.append(f"- {src} --[{edge_type}]--> {dst}")

        return "\n".join(context_parts)

    async def _generate_with_context(self, question: str, context: str) -> str:
        """Генерация ответа через LLM с контекстом"""
        system_prompt = """
Ты — эксперт с доступом к базе знаний MemoryX.
Используй предоставленный контекст для ответа.
Если контекста недостаточно, укажи это.
Всегда упоминай источники (атомы) из MemoryX.
"""

        user_prompt = f"""
Контекст из MemoryX:
{context}

Вопрос: {question}

Ответ:
"""

        response = await self.llm.chat.completions.create(
            model=self.model,
            messages=[
                {"role": "system", "content": system_prompt},
                {"role": "user", "content": user_prompt},
            ],
            temperature=0.7,
            max_tokens=1000,
        )

        return response.choices[0].message.content


# ============================================================================
# Advanced: Fine-tuning на MemoryX
# ============================================================================


class MemoryXFineTuner:
    """
    Fine-tuning LLM на знаниях из MemoryX

    Использует:
    - Batch export атомов
    - Генерация QA пар
    - Fine-tuning модели
    """

    def __init__(self, memoryx_client: MemoryXClient):
        self.memoryx = memoryx_client

    async def export_training_data(
        self, output_file: str = "training_data.jsonl", limit: int = 1000
    ) -> int:
        """
        Экспорт знаний из MemoryX в формат для fine-tuning

        Args:
            output_file: Файл для сохранения
            limit: Максимальное количество записей

        Returns:
            Количество экспортированных записей
        """
        # Query для получения всех знаний (по типам)
        all_atoms = []

        for atom_type in ["DEFINITION", "FACT", "RULE", "PROCEDURE"]:
            result = await self.memoryx.query(f"Все {atom_type}")
            atoms = result.get("atoms", [])
            all_atoms.extend(atoms)

        # Генерация QA пар
        training_data = []
        for atom in all_atoms[:limit]:
            content = atom.get("content", "")
            atom_type = atom.get("type", "FACT")

            # Создаём вопрос из атома
            question = self._generate_question(content, atom_type)

            training_data.append(
                {
                    "messages": [
                        {"role": "user", "content": question},
                        {"role": "assistant", "content": content},
                    ]
                }
            )

        # Сохранение
        with open(output_file, "w", encoding="utf-8") as f:
            for item in training_data:
                f.write(json.dumps(item, ensure_ascii=False) + "\n")

        return len(training_data)

    def _generate_question(self, content: str, atom_type: str) -> str:
        """Генерация вопроса из атома"""
        if atom_type == "DEFINITION":
            # "Rust - системный язык" -> "Что такое Rust?"
            parts = content.split(" - ")
            if len(parts) >= 2:
                return f"Что такое {parts[0]}?"
            return f"Определи: {content}"

        elif atom_type == "FACT":
            return f"Верно ли что: {content}"

        elif atom_type == "RULE":
            return f"Какое правило применяется?"

        return f"Расскажи о: {content}"


# ============================================================================
# Пример использования
# ============================================================================


async def main():
    """Пример использования MemoryX с LLM"""

    # Инициализация
    memoryx = MemoryXClient(data_dir="./memoryx_data")
    await memoryx.connect()

    llm = AsyncOpenAI(api_key="your-api-key")

    rag = MemoryXRAG(memoryx, llm)

    # RAG query
    question = "Как работает ownership в Rust?"
    answer = await rag.generate_answer(question)

    print(f"Вопрос: {question}")
    print(f"Ответ: {answer}")
    print()

    # Fine-tuning export
    tuner = MemoryXFineTuner(memoryx)
    count = await tuner.export_training_data(limit=500)
    print(f"Экспортировано {count} записей для fine-tuning")


if __name__ == "__main__":
    asyncio.run(main())
