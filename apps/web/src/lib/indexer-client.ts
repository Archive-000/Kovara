export interface IndexerQueryOptions {
  endpoint: string;
  method?: 'GET' | 'POST' | 'PUT' | 'DELETE';
  params?: Record<string, any>;
  body?: any;
  token?: string;
}

export class IndexerClientError extends Error {
  constructor(public statusCode: number, message: string) {
    super(message);
    this.name = 'IndexerClientError';
  }
}

export class IndexerClient {
  private baseUrl: string;

  constructor(baseUrl: string = process.env.NEXT_PUBLIC_INDEXER_API_URL || 'https://indexer.nexafx.com/v1') {
    this.baseUrl = baseUrl;
  }

  public async request<T>(options: IndexerQueryOptions): Promise<T> {
    const { endpoint, method = 'GET', params, body, token } = options;

    let url = `${this.baseUrl}/${endpoint.replace(/^\/+/, '')}`;
    if (params) {
      const searchParams = new URLSearchParams();
      Object.entries(params).forEach(([key, value]) => {
        if (value !== undefined && value !== null) {
          if (Array.isArray(value)) {
            value.forEach((item) => searchParams.append(key, String(item)));
          } else {
            searchParams.append(key, String(value));
          }
        }
      });
      const queryString = searchParams.toString();
      if (queryString) {
        url += `?${queryString}`;
      }
    }

    const headers: Record<string, string> = {
      'Content-Type': 'application/json',
    };

    if (token) {
      headers['Authorization'] = `Bearer ${token}`;
    }

    const response = await fetch(url, {
      method,
      headers,
      body: body ? JSON.stringify(body) : undefined,
    });

    if (!response.ok) {
      let errorMessage = `Indexer request failed with status ${response.status}`;
      try {
        const errorBody = await response.json();
        errorMessage = errorBody.message || errorMessage;
      } catch (_) {
        // Fallback if error payload is not JSON
      }
      throw new IndexerClientError(response.status, errorMessage);
    }

    return response.json() as Promise<T>;
  }
}

export const indexerClient = new IndexerClient();